"""Tests for plan manifest generation, loading, and identity verification.

Requires a release build (`cargo build --release`) for the CLI binary
and the shared library.

Verifies:
    * CLI `plan` emits a versioned manifest for every source backend
    * Manifest round-trip: load, verify, exact range coverage
    * Range construction: records are never split
    * Identity rejection: size change, same-size content change
    * Structural validation of malformed manifests
"""

from __future__ import annotations

import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from native_io.plan_manifest import (
    PlanFormatError,
    PlanIdentityMismatch,
    load_plan,
    load_verified_plan,
    parse_manifest,
    verify_plan,
)

_PROJECT_ROOT = pathlib.Path(__file__).resolve().parents[1]


def _cli_path() -> pathlib.Path | None:
    name = "mmap-chunker.exe" if platform.system() == "Windows" else "mmap-chunker"
    candidate = _PROJECT_ROOT / "target" / "release" / name
    return candidate if candidate.is_file() else None


CLI = _cli_path()


def _records(count: int, delimiter: bytes = b"\r\n") -> bytes:
    return b"".join(
        ("record-%05d,payload-%05d" % (index, index)).encode() + delimiter
        for index in range(count)
    )


@unittest.skipUnless(CLI is not None, "release CLI binary not built")
class TestPlanManifest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmpdir = pathlib.Path(tempfile.mkdtemp(prefix="plan_manifest_tests_"))

    def tearDown(self) -> None:
        shutil.rmtree(self._tmpdir, ignore_errors=True)

    def _write(self, name: str, content: bytes) -> pathlib.Path:
        path = self._tmpdir / name
        path.write_bytes(content)
        return path

    def _plan(
        self,
        source: pathlib.Path,
        *,
        parts: str = "8",
        delimiter_hex: str | None = "0d0a",
        source_mode: str | None = None,
        window: str | None = None,
        output: pathlib.Path | None = None,
        extra_args: list[str] | None = None,
    ) -> pathlib.Path:
        arguments = [
            str(CLI),
            "plan",
            str(source),
            "--parts",
            parts,
        ]
        if delimiter_hex is not None:
            arguments += ["--delimiter-hex", delimiter_hex]
        if source_mode is not None:
            arguments += ["--source", source_mode]
        if window is not None:
            arguments += ["--window", window]
        if extra_args:
            arguments += extra_args
        manifest = output or (source.with_suffix(source.suffix + ".plan.json"))
        arguments += ["--output", str(manifest)]
        completed = subprocess.run(
            arguments, capture_output=True, check=False, timeout=120
        )
        self.assertEqual(
            completed.returncode,
            0,
            "plan CLI failed: %s" % completed.stderr.decode(errors="replace"),
        )
        return manifest

    def test_roundtrip_verifies_and_covers_records(self) -> None:
        content = _records(500)
        source = self._write("roundtrip.log", content)
        manifest = self._plan(source, parts="8")

        plan = load_verified_plan(manifest, source)
        self.assertEqual(plan.schema_version, 1)
        self.assertEqual(plan.delimiter, b"\r\n")
        self.assertEqual(plan.source_mode, "mmap")
        self.assertEqual(plan.window_bytes, None)
        self.assertEqual(plan.requested_partitions, 8)
        self.assertGreaterEqual(plan.actual_partitions, 1)
        self.assertGreater(plan.source_size, 0)

        chunks = []
        cursor = 0
        for byte_range in plan.ranges:
            self.assertEqual(byte_range.start, cursor)
            self.assertGreater(byte_range.length, 0)
            chunks.append(content[byte_range.start : byte_range.end])
            cursor = byte_range.end
        self.assertEqual(cursor, len(content))
        self.assertEqual(b"".join(chunks), content)
        for byte_range in plan.ranges[:-1]:
            self.assertTrue(
                content[byte_range.end - 2 : byte_range.end] == b"\r\n",
                "range split a CRLF record",
            )

    def test_all_source_modes_produce_same_ranges(self) -> None:
        content = _records(300)
        source = self._write("modes.log", content)

        baseline = load_plan(self._plan(source, parts="7"))
        for source_mode, window in (("windowed", "65536"), ("pread", None)):
            manifest = self._plan(
                source,
                parts="7",
                source_mode=source_mode,
                window=window,
                output=self._tmpdir / ("plan-%s.json" % source_mode),
            )
            plan = load_verified_plan(manifest, source)
            self.assertEqual(plan.source_mode, source_mode)
            self.assertEqual(
                [range_ for range_ in plan.ranges],
                [range_ for range_ in baseline.ranges],
                "source mode %s diverged" % source_mode,
            )

    def test_append_rejects_plan(self) -> None:
        source = self._write("append.log", _records(50))
        manifest = self._plan(source)
        plan = load_plan(manifest)

        with source.open("ab") as handle:
            handle.write(b"late-record\r\n")

        with self.assertRaises(PlanIdentityMismatch):
            verify_plan(plan, source)

    def test_same_size_content_change_rejects_plan(self) -> None:
        content = bytearray(_records(50))
        source = self._write("same_size.log", bytes(content))
        manifest = self._plan(source)
        plan = load_plan(manifest)

        content[0] = ord("X") if content[0] != ord("X") else ord("Y")
        source.write_bytes(bytes(content))

        with self.assertRaises(PlanIdentityMismatch):
            verify_plan(plan, source)

    def test_metadata_rejection_when_fingerprint_still_matches(self) -> None:
        content = _records(50)
        source = self._write("mtime.log", content)
        manifest = self._plan(source)
        plan = load_plan(manifest)

        # Same content and size, different mtime.
        os.utime(source, (1_000_000, 1_000_000))

        with self.assertRaises(PlanIdentityMismatch):
            verify_plan(plan, source)

        # Disabling metadata checks accepts it again.
        verify_plan(plan, source, check_metadata=False)

    def test_fixed_width_plan_framing_roundtrip(self) -> None:
        content = bytes(range(100))
        source = self._write("fixed.bin", content)
        manifest = self._plan(
            source,
            parts="4",
            delimiter_hex=None,
            extra_args=["--framing", "fixed", "--record-bytes", "16"],
        )

        plan = load_verified_plan(manifest, source)
        self.assertEqual(plan.framing_strategy, "fixed_width")
        self.assertEqual(plan.framing["record_bytes"], 16)
        self.assertEqual(plan.delimiter, None)
        self.assertEqual(
            [(byte_range.start, byte_range.end) for byte_range in plan.ranges],
            [(0, 32), (32, 64), (64, 80), (80, 100)],
        )
        for byte_range in plan.ranges[:-1]:
            self.assertEqual(byte_range.end % 16, 0)

    def test_length_prefixed_plan_framing_roundtrip(self) -> None:
        payloads = [b"alpha", b"be", b"gamma-payload", b""]
        content = b"".join(
            len(payload).to_bytes(2, "little") + payload for payload in payloads
        )
        source = self._write("length.bin", content)
        manifest = self._plan(
            source,
            parts="2",
            delimiter_hex=None,
            extra_args=[
                "--framing",
                "length-prefixed",
                "--prefix-bytes",
                "2",
            ],
        )

        plan = load_verified_plan(manifest, source)
        self.assertEqual(plan.framing_strategy, "length_prefixed")
        self.assertEqual(plan.framing["prefix_bytes"], 2)
        self.assertTrue(plan.framing["little_endian"])
        self.assertFalse(plan.framing["length_includes_prefix"])
        self.assertEqual(
            [(byte_range.start, byte_range.end) for byte_range in plan.ranges],
            [(0, 26), (26, 28)],
        )

    def test_indexed_plan_uses_source_index_reference(self) -> None:
        content = _records(120)
        source = self._write("indexed.log", content)

        index_path = source.with_suffix(source.suffix + ".mmapidx")
        completed = subprocess.run(
            [
                str(CLI),
                "index",
                str(source),
                "--every",
                "4",
                "--delimiter-hex",
                "0d0a",
            ],
            capture_output=True,
            check=False,
            timeout=120,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

        manifest = self._plan(
            source,
            parts="6",
            delimiter_hex=None,
            extra_args=["--index", str(index_path)],
        )
        plan = load_verified_plan(manifest, source)
        self.assertIsNone(plan.source_mode)
        self.assertIsNotNone(plan.source_index)
        source_index = plan.source_index or {}
        self.assertEqual(source_index["stride"], 4)
        self.assertEqual(source_index["record_count"], 120)
        self.assertEqual(source_index["path"], str(index_path))
        for byte_range in plan.ranges[:-1]:
            self.assertEqual(byte_range.start % 28, 0)
            self.assertEqual(content[byte_range.end - 2 : byte_range.end], b"\r\n")

    def test_malformed_manifest_is_rejected(self) -> None:
        document = {
            "schema": "mmap-chunker-plan",
            "schema_version": 1,
            "planner": {"name": "mmap-chunker-core", "version": "0.0.0"},
            "source": {"path": "x", "size": 4, "identity": {}},
            "framing": {
                "strategy": "delimiter",
                "delimiter_hex": "0d0a",
                "delimiter_len": 2,
            },
            "partitioning": {
                "strategy": "bytes",
                "requested_partitions": 2,
                "actual_partitions": 2,
                "source_mode": "mmap",
                "window_bytes": None,
            },
            "ranges": [
                {"index": 0, "start": 0, "end": 2, "length": 2},
                {"index": 1, "start": 3, "end": 4, "length": 1},
            ],
        }
        plan = parse_manifest(json.dumps(document))
        with self.assertRaises(PlanFormatError):
            from native_io.plan_manifest import verify_ranges

            verify_ranges(plan)

    def test_duplicate_top_level_key_is_rejected(self) -> None:
        text = '{"schema": "other", "schema": "mmap-chunker-plan"}'
        with self.assertRaises(PlanFormatError):
            parse_manifest(text)

    def test_duplicate_nested_key_is_rejected(self) -> None:
        document = {
            "schema": "mmap-chunker-plan",
            "schema_version": 1,
            "planner": {"name": "mmap-chunker-core", "version": "0.0.0"},
            "source": {"path": "x", "size": 4, "identity": {}},
            "framing": {
                "strategy": "delimiter",
                "delimiter_hex": "0d0a",
                "delimiter_len": 2,
            },
            "partitioning": {
                "strategy": "bytes",
                "requested_partitions": 2,
                "actual_partitions": 2,
                "source_mode": "mmap",
                "window_bytes": None,
            },
            "ranges": [
                {"index": 0, "start": 0, "end": 2, "length": 2},
                {"index": 1, "start": 3, "end": 4, "length": 1},
            ],
        }
        text = json.dumps(document)
        tampered = text.replace(
            '"delimiter_hex": "0d0a"',
            '"delimiter_hex": "0d0a", "delimiter_hex": "0a"',
            1,
        )
        self.assertNotEqual(tampered, text)
        with self.assertRaises(PlanFormatError):
            parse_manifest(tampered)

    def test_wrong_schema_version_is_rejected(self) -> None:
        source = self._write("schema.log", _records(10))
        manifest = self._plan(source)
        document = json.loads(manifest.read_text(encoding="utf-8"))
        document["schema_version"] = 99
        with self.assertRaises(PlanFormatError):
            parse_manifest(json.dumps(document))

    def test_range_length_mismatch_is_rejected(self) -> None:
        source = self._write("length.log", _records(10))
        manifest = self._plan(source)
        document = json.loads(manifest.read_text(encoding="utf-8"))
        document["ranges"][0]["length"] += 1
        with self.assertRaises(PlanFormatError):
            parse_manifest(json.dumps(document))

    def test_empty_source_plan(self) -> None:
        source = self._write("empty.log", b"")
        manifest = self._plan(source)
        plan = load_verified_plan(manifest, source)
        self.assertEqual(plan.ranges, [])
        self.assertEqual(plan.actual_partitions, 0)
        self.assertEqual(plan.source_size, 0)


if __name__ == "__main__":
    unittest.main()
