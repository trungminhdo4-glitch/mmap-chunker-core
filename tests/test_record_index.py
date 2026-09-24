"""Tests for sparse record-index sidecars (native_io.record_index).

Requires a release build (`cargo build --release`) for the CLI binary.

Verifies:
    * CLI `index` writes a versioned sidecar with identity and framing
    * Python index verification rejects changed or truncated sources
    * Python index-derived planning matches the Rust planner exactly
    * Malformed indexes are rejected structurally
"""

from __future__ import annotations

import json
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from native_io.plan_manifest import PlanFormatError, PlanIdentityMismatch
from native_io.record_index import (
    INDEX_SCHEMA,
    load_index,
    load_verified_index,
    parse_index,
    plan_partition_boundaries_from_index,
    verify_index,
)

_PROJECT_ROOT = pathlib.Path(__file__).resolve().parents[1]


def _cli_path() -> pathlib.Path | None:
    name = "mmap-chunker.exe" if platform.system() == "Windows" else "mmap-chunker"
    candidate = _PROJECT_ROOT / "target" / "release" / name
    return candidate if candidate.is_file() else None


CLI = _cli_path()


def _records(count: int, payload: bytes = b"\r\n") -> bytes:
    return b"".join(
        ("record-%05d,payload-%05d" % (index, index)).encode() + payload
        for index in range(count)
    )


@unittest.skipUnless(CLI is not None, "release CLI binary not built")
class TestRecordIndex(unittest.TestCase):
    def setUp(self) -> None:
        self._tmpdir = pathlib.Path(tempfile.mkdtemp(prefix="record_index_tests_"))

    def tearDown(self) -> None:
        shutil.rmtree(self._tmpdir, ignore_errors=True)

    def _write(self, name: str, content: bytes) -> pathlib.Path:
        path = self._tmpdir / name
        path.write_bytes(content)
        return path

    def _run(self, arguments: list[str]) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            [str(CLI), *arguments], capture_output=True, check=False, timeout=120
        )

    def _index(
        self, source: pathlib.Path, *, every: str = "4", extra: list[str] | None = None
    ) -> pathlib.Path:
        arguments = [
            "index",
            str(source),
            "--every",
            every,
            "--delimiter-hex",
            "0d0a",
        ]
        if extra:
            arguments += extra
        completed = self._run(arguments)
        self.assertEqual(
            completed.returncode,
            0,
            "index CLI failed: %s" % completed.stderr.decode(errors="replace"),
        )
        return source.with_suffix(source.suffix + ".mmapidx")

    def test_build_load_verify(self) -> None:
        content = _records(200)
        source = self._write("records.log", content)
        index_path = self._index(source, every="4")

        index = load_verified_index(index_path, source)
        self.assertEqual(index.schema_version, 1)
        self.assertEqual(index.generator_name, "mmap-chunker-core")
        self.assertEqual(index.stride, 4)
        self.assertEqual(index.record_count, 200)
        self.assertEqual(index.framing_strategy, "delimiter")
        self.assertEqual(index.framing["delimiter_hex"], "0d0a")
        self.assertEqual(index.source_size, len(content))
        self.assertEqual(len(index.offsets), 50)
        self.assertEqual(index.offsets[0], 0)

        # Offsets are exact record starts.
        record_size = 28
        for offset in index.offsets[1:]:
            self.assertEqual(offset % record_size, 0)
            self.assertEqual(content[offset - 2 : offset], b"\r\n")

        # JSON round trip.
        reparsed = parse_index(index_path.read_text(encoding="utf-8"))
        self.assertEqual(reparsed, index)

    def test_identity_mismatch_after_change(self) -> None:
        content = _records(50)
        source = self._write("changed.log", content)
        index_path = self._index(source, every="1")
        index = load_index(index_path)

        with source.open("ab") as handle:
            handle.write(b"late-record\r\n")
        with self.assertRaises(PlanIdentityMismatch):
            verify_index(index, source)

    def test_python_planning_matches_cli_manifest(self) -> None:
        content = _records(300)
        source = self._write("parity.log", content)
        index_path = self._index(source, every="5")
        index = load_verified_index(index_path, source)

        expected = plan_partition_boundaries_from_index(index, 7, len(content))
        manifest_path = self._tmpdir / "parity-plan.json"
        completed = self._run(
            [
                "plan",
                str(source),
                "--parts",
                "7",
                "--index",
                str(index_path),
                "--output",
                str(manifest_path),
            ]
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        document = json.loads(manifest_path.read_text(encoding="utf-8"))
        actual = [(entry["start"], entry["end"]) for entry in document["ranges"]]
        self.assertEqual(actual, expected)

        # Coverage and alignment.
        cursor = 0
        for start, end in actual:
            self.assertEqual(start, cursor)
            self.assertGreater(end, start)
            self.assertEqual(start % 28, 0)
            cursor = end
        self.assertEqual(cursor, len(content))

    def test_python_planning_state_does_not_rescan(self) -> None:
        content = _records(10)
        source = self._write("tiny.log", content)
        index_path = self._index(source, every="3")
        index = load_verified_index(index_path, source)

        self.assertEqual(
            plan_partition_boundaries_from_index(index, 1, len(content)),
            [(0, len(content))],
        )
        self.assertEqual(
            plan_partition_boundaries_from_index(index, 0, len(content)), []
        )
        with self.assertRaises(PlanFormatError):
            plan_partition_boundaries_from_index(index, 4, len(content) + 1)

    def test_malformed_index_is_rejected(self) -> None:
        valid = {
            "schema": INDEX_SCHEMA,
            "schema_version": 1,
            "generator": {"name": "mmap-chunker-core", "version": "0.0.0"},
            "source": {
                "path": "x",
                "size": 40,
                "identity": {
                    "modified_unix_nanos": None,
                    "device": None,
                    "inode": None,
                    "sample_fingerprint": None,
                    "sample_bytes": 65536,
                },
            },
            "framing": {
                "strategy": "delimiter",
                "delimiter_hex": "0d0a",
                "delimiter_len": 2,
            },
            "index": {"stride": 2, "record_count": 4, "record_offsets": [0, 20]},
        }

        mutation_cases = [
            lambda document: document.update({"schema": "other"}),
            lambda document: document.update({"schema_version": 99}),
            lambda document: document["index"].update({"stride": 0}),
            lambda document: document["index"].update({"record_offsets": [0, 0]}),
            lambda document: document["index"].update({"record_offsets": [20]}),
            lambda document: document["index"].update(
                {"record_offsets": [0, 20, 30, 35]}
            ),
            lambda document: document["index"].update({"record_offsets": [0, 99]}),
        ]
        for mutate in mutation_cases:
            document = json.loads(json.dumps(valid))
            mutate(document)
            with self.assertRaises(PlanFormatError):
                parse_index(json.dumps(document))

    def test_fixed_width_index(self) -> None:
        content = bytes(range(100))
        source = self._write("fixed.bin", content)
        completed = self._run(
            [
                "index",
                str(source),
                "--every",
                "2",
                "--framing",
                "fixed",
                "--record-bytes",
                "16",
            ]
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

        index_path = source.with_suffix(source.suffix + ".mmapidx")
        index = load_verified_index(index_path, source)
        self.assertEqual(index.framing_strategy, "fixed_width")
        self.assertEqual(index.record_count, 7)
        self.assertEqual(index.offsets, [0, 32, 64, 96])


if __name__ == "__main__":
    unittest.main()
