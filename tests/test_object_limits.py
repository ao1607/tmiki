"""Limits on objects received and read: JSON nesting, chunk manifests,
decompression bombs. A hostile remote sends objects tmi's own commands would
never write; each is refused with a normal error before it is stored.

The Rust side's twins are rust/crates/tmi-store (json.rs, store.rs) and
rust/crates/tmi-transport/src/rawobj.rs.
"""
from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
import zlib
from pathlib import Path

from tmi import objects
from tmi.objects import ObjectStore


def pack(object_type: str, payload: bytes, level: int = 6) -> tuple[str, bytes]:
    data = object_type.encode() + b"\0" + payload
    return hashlib.sha256(data).hexdigest(), zlib.compress(data, level)


class ObjectLimitTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = ObjectStore(Path(self._tmp.name) / "objects")

    def refuse(self, object_type: str, payload: bytes, text: str) -> None:
        oid, raw = pack(object_type, payload)
        with self.assertRaises(ValueError) as caught:
            self.store.write_raw(oid, raw)
        self.assertIn(text, str(caught.exception))
        self.assertFalse(self.store.path_for(oid).exists())

    def test_deep_json_is_refused_not_a_recursion_error(self) -> None:
        for kind in ("commit", "tree", "chunked"):
            self.refuse(kind, b"[" * 3000 + b"]" * 3000, "nested deeper than 256")
        self.refuse("tree", b'{"a":' * 300 + b"1" + b"}" * 300, "nested deeper than 256")

    def test_depth_rule_edges(self) -> None:
        self.assertFalse(objects.json_depth_exceeds(b"[" * 256 + b"]" * 256))
        self.assertTrue(objects.json_depth_exceeds(b"[" * 257 + b"]" * 257))
        # brackets inside strings do not count
        self.assertFalse(objects.json_depth_exceeds(b'["' + b"[" * 1000 + b'\\"[["]'))
        with self.assertRaises(objects.JsonTooDeepError):
            objects.parse_json_bytes(b"[" * 3000)

    def test_normal_objects_still_stored(self) -> None:
        oid, raw = pack("commit", json.dumps({"a": [[[1]]]}).encode())
        self.store.write_raw(oid, raw)
        self.assertEqual(self.store.read_json(oid, "commit"), {"a": [[[1]]]})

    def manifest(self, **fields) -> bytes:
        return json.dumps({"size": 0, "glue": None, "parts": [], **fields}).encode()

    def test_chunk_manifest_is_checked_on_receive(self) -> None:
        good = hashlib.sha256(b"x").hexdigest()
        self.refuse("chunked", self.manifest(size=-1), "bad size")
        self.refuse("chunked", self.manifest(size=objects.MAX_CHUNKED_SIZE + 1), "bad size")
        self.refuse("chunked", self.manifest(parts=["../../x"]), "not an object id")
        self.refuse("chunked", self.manifest(parts=[good] * (objects.MAX_CHUNK_PARTS + 1), size=10), "bad part list")
        self.refuse("chunked", self.manifest(size=5, parts=[10]), "more glue than its size")
        self.refuse("chunked", self.manifest(parts=[1.5]), "bad part")

    def test_repeated_part_stops_at_declared_size(self) -> None:
        part = self.store.write("blob", b"\0" * 100_000)
        manifest = {"size": 100_000, "glue": None, "parts": [part] * 5000}
        oid = self.store.write_json("chunked", manifest)
        with self.assertRaises(ValueError) as caught:
            self.store.read_blob(oid)
        self.assertIn("more than 100000 bytes", str(caught.exception))

    def test_nested_chunked_part_is_refused(self) -> None:
        inner_part = self.store.write("blob", b"abc")
        inner = self.store.write_json("chunked", {"size": 3, "glue": None, "parts": [inner_part]})
        outer = self.store.write_json("chunked", {"size": 3, "glue": None, "parts": [inner]})
        with self.assertRaises(ValueError) as caught:
            self.store.read_blob(outer)
        self.assertIn("expected blob", str(caught.exception))

    def test_honest_chunked_blob_reads_back(self) -> None:
        a = self.store.write("blob", b"a" * 10)
        b = self.store.write("blob", b"b" * 7)
        glue = self.store.write("blob", b"ZZ")
        oid = self.store.write_json("chunked", {"size": 29, "glue": glue, "parts": [a, 1, b, a, 1]})
        self.assertEqual(self.store.read_blob(oid), b"a" * 10 + b"Z" + b"b" * 7 + b"a" * 10 + b"Z")

    def test_decompression_bomb_is_refused_while_inflating(self) -> None:
        # 33 MiB of zeros as a commit: past the 32 MiB cap for JSON objects
        payload = b"0" * (objects.MAX_META_OBJECT_SIZE + 1)
        self.refuse("commit", payload, "larger than")
        # a blob over its cap, built cheaply: shrink the cap for the test
        saved = objects.MAX_BLOB_SIZE
        objects.MAX_BLOB_SIZE = 1 << 20
        self.addCleanup(setattr, objects, "MAX_BLOB_SIZE", saved)
        self.refuse("blob", b"\0" * ((1 << 20) + 1), "larger than")
        oid, raw = pack("blob", b"\0" * (1 << 20))
        self.store.write_raw(oid, raw)

    def test_oversize_object_on_disk_is_refused_on_read(self) -> None:
        payload = b"0" * (objects.MAX_META_OBJECT_SIZE + 1)
        oid, raw = pack("tree", payload)
        path = self.store.path_for(oid)
        path.parent.mkdir(parents=True)
        path.write_bytes(raw)
        with self.assertRaises(ValueError):
            self.store.read(oid)


if __name__ == "__main__":
    unittest.main()
