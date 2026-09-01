"""Tests for native_io — agnostic byte chunking providers.

Verifies:
    * Both providers satisfy the ByteChunkProvider protocol
    * Core invariant: native_backend(input) == baseline_backend(input)
    * Chunk coverage (no gaps, no overlaps, total = file size)
    * Empty file handling
    * No-delimiter handling
    * Small/large chunk sizes
    * Consecutive delimiters
    * No trailing newline
    * Out-of-bounds index
    * scan-before-open error
    * Repeated scans produce same result
    * Context manager protocol
    * Provider discovery
    * Provider selection (all modes)
    * Fallback when mmap unavailable
    * Shadow comparison
    * Protocol isinstance check
"""

from __future__ import annotations

import os
import pathlib
import shutil
import tempfile
import unittest

import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from native_io import (
    ByteChunkProvider,
    PythonChunkProvider,
    create_provider,
    discover_mmap_provider,
    select_provider,
    shadow_compare,
)

MMAP_AVAILABLE = discover_mmap_provider().available


class TestFileFixture:
    """Mixin providing temporary file helpers."""

    _tmpdir: str | None = None
    _paths: list[str]

    def setUp(self) -> None:
        self._tmpdir = tempfile.mkdtemp(prefix="native_io_tests_")
        self._paths = []

    def tearDown(self) -> None:
        if self._tmpdir is not None:
            shutil.rmtree(self._tmpdir, ignore_errors=True)

    def _make_file(self, name: str, content: bytes) -> str:
        path = os.path.join(self._tmpdir, name)  # type: ignore[arg-type]
        with open(path, "wb") as f:
            f.write(content)
        self._paths.append(path)
        return path


class TestPythonChunkProvider(TestFileFixture, unittest.TestCase):
    def test_empty_file(self) -> None:
        path = self._make_file("empty.txt", b"")
        doc = PythonChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(), 0)
        self.assertEqual(doc.chunk_count, 0)
        doc.close()

    def test_small_file(self) -> None:
        content = b"hello\nworld\n"
        path = self._make_file("small.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(chunk_size=1024), 1)
        self.assertEqual(doc.get_chunk(0), content)
        doc.close()

    def test_chunk_coverage(self) -> None:
        data = b"aaa\nbbb\nccc\nddd\neee\n"
        path = self._make_file("coverage.txt", data)
        doc = PythonChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=6)
        total = 0
        for i in range(doc.chunk_count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(data))
        doc.close()

    def test_sequential_order(self) -> None:
        lines = [("line%d\n" % i).encode() for i in range(10)]
        content = b"".join(lines)
        path = self._make_file("sequential.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=8)
        current = 0
        for i in range(doc.chunk_count):
            chunk = doc.get_chunk(i)
            self.assertEqual(
                chunk,
                content[current : current + len(chunk)],
            )
            current += len(chunk)
        self.assertEqual(current, len(content))
        doc.close()

    def test_no_delimiter(self) -> None:
        content = b"no_newlines_here_at_all"
        path = self._make_file("nodelim.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(chunk_size=5), 1)
        self.assertEqual(doc.get_chunk(0), content)
        doc.close()

    def test_no_trailing_newline(self) -> None:
        content = b"line1\nline2\nline3"
        path = self._make_file("notrail.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=4)
        total = 0
        for i in range(doc.chunk_count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(content))
        doc.close()

    def test_chunk_size_zero(self) -> None:
        content = b"abc\n"
        path = self._make_file("zero.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        self.assertGreater(doc.scan(chunk_size=0), 0)
        doc.close()

    def test_binary_with_nul(self) -> None:
        content = b"prefix\x00suffix\n"
        path = self._make_file("binary.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(chunk_size=100), 1)
        self.assertEqual(doc.get_chunk(0), content)
        doc.close()

    def test_only_newlines(self) -> None:
        content = b"\n\n\n"
        path = self._make_file("newlines.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        count = doc.scan(chunk_size=1)
        total = 0
        for i in range(count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(content))
        doc.close()

    def test_consecutive_delimiters(self) -> None:
        content = b"line1\n\n\nline2\n"
        path = self._make_file("consecutive.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        count = doc.scan(chunk_size=6)
        total = 0
        for i in range(count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(content))
        doc.close()

    def test_record_larger_than_chunk(self) -> None:
        content = b"short\nverylonglinewithodelimiteratall\nshort\n"
        path = self._make_file("longrecord.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        count = doc.scan(chunk_size=6)
        total = 0
        for i in range(count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(content))
        doc.close()

    def test_out_of_bounds(self) -> None:
        content = b"hello\n"
        path = self._make_file("oob.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=1024)
        with self.assertRaises(IndexError):
            doc.get_chunk(1)
        with self.assertRaises(IndexError):
            doc.get_chunk(-1)
        doc.close()

    def test_scan_before_open(self) -> None:
        doc = PythonChunkProvider()
        with self.assertRaises(RuntimeError):
            doc.scan(chunk_size=64)

    def test_repeated_scan(self) -> None:
        content = b"a\nb\nc\nd\ne\n"
        path = self._make_file("repeat.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        count1 = doc.scan(chunk_size=4)
        chunks1 = [doc.get_chunk(i) for i in range(count1)]
        count2 = doc.scan(chunk_size=4)
        chunks2 = [doc.get_chunk(i) for i in range(count2)]
        self.assertEqual(count1, count2)
        self.assertEqual(chunks1, chunks2)
        doc.close()

    def test_context_manager(self) -> None:
        content = b"test\n"
        path = self._make_file("cm.txt", content)
        with PythonChunkProvider() as doc:
            doc.open(path)
            self.assertEqual(doc.scan(), 1)


class TestMmapChunkProvider(TestFileFixture, unittest.TestCase):
    def _require_mmap(self) -> None:
        if not MMAP_AVAILABLE:
            self.skipTest("mmap provider not available")

    def setUp(self) -> None:
        super().setUp()
        self._require_mmap()

    def test_empty_file(self) -> None:
        path = self._make_file("empty.txt", b"")
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(), 0)
        self.assertEqual(doc.chunk_count, 0)
        doc.close()

    def test_small_file(self) -> None:
        content = b"hello\nworld\n"
        path = self._make_file("small.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(chunk_size=1024), 1)
        self.assertEqual(doc.get_chunk(0), content)
        doc.close()

    def test_chunk_coverage(self) -> None:
        data = b"aaa\nbbb\nccc\nddd\neee\n"
        path = self._make_file("coverage.txt", data)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=6)
        total = 0
        for i in range(doc.chunk_count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(data))
        doc.close()

    def test_sequential_order(self) -> None:
        lines = [("line%d\n" % i).encode() for i in range(10)]
        content = b"".join(lines)
        path = self._make_file("sequential.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=8)
        current = 0
        for i in range(doc.chunk_count):
            chunk = doc.get_chunk(i)
            self.assertEqual(
                chunk,
                content[current : current + len(chunk)],
            )
            current += len(chunk)
        self.assertEqual(current, len(content))
        doc.close()

    def test_no_delimiter(self) -> None:
        content = b"no_newlines_here_at_all"
        path = self._make_file("nodelim.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        self.assertEqual(doc.scan(chunk_size=5), 1)
        self.assertEqual(doc.get_chunk(0), content)
        doc.close()

    def test_no_trailing_newline(self) -> None:
        content = b"line1\nline2\nline3"
        path = self._make_file("notrail.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=4)
        total = 0
        for i in range(doc.chunk_count):
            total += len(doc.get_chunk(i))
        self.assertEqual(total, len(content))
        doc.close()

    def test_out_of_bounds(self) -> None:
        content = b"hello\n"
        path = self._make_file("oob.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=1024)
        with self.assertRaises(IndexError):
            doc.get_chunk(1)
        with self.assertRaises(IndexError):
            doc.get_chunk(-1)
        doc.close()

    def test_scan_before_open(self) -> None:
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        with self.assertRaises(RuntimeError):
            doc.scan(chunk_size=64)

    def test_repeated_scan(self) -> None:
        content = b"a\nb\nc\nd\ne\n"
        path = self._make_file("repeat.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        count1 = doc.scan(chunk_size=4)
        chunks1 = [doc.get_chunk(i) for i in range(count1)]
        count2 = doc.scan(chunk_size=4)
        chunks2 = [doc.get_chunk(i) for i in range(count2)]
        self.assertEqual(count1, count2)
        self.assertEqual(chunks1, chunks2)
        doc.close()

    def test_context_manager(self) -> None:
        content = b"test\n"
        path = self._make_file("cm.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        with MmapChunkProvider() as doc:
            doc.open(path)
            self.assertEqual(doc.scan(), 1)

    def test_unsupported_delimiter(self) -> None:
        content = b"hello,world\n"
        path = self._make_file("comma.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        doc.open(path)
        with self.assertRaises(ValueError):
            doc.scan(chunk_size=1024, delimiter=b",")

    def test_nonexistent_file(self) -> None:
        from native_io.mmap_provider import MmapChunkProvider

        doc = MmapChunkProvider()
        with self.assertRaises(OSError):
            doc.open(os.path.join(self._tmpdir, "nonexistent.txt"))  # type: ignore[arg-type]


class TestInvariant(TestFileFixture, unittest.TestCase):
    """Core invariant: native_backend(input) == baseline_backend(input)."""

    def _require_mmap(self) -> None:
        if not MMAP_AVAILABLE:
            self.skipTest("mmap provider not available")

    def setUp(self) -> None:
        super().setUp()
        self._require_mmap()

    def _compare(self, content: bytes, chunk_size: int) -> None:
        path = self._make_file("invariant.txt", content)
        from native_io.mmap_provider import MmapChunkProvider

        py = PythonChunkProvider()
        py.open(path)
        py.scan(chunk_size=chunk_size)
        py_chunks = [py.get_chunk(i) for i in range(py.chunk_count)]
        py.close()

        mmap = MmapChunkProvider()
        mmap.open(path)
        mmap.scan(chunk_size=chunk_size)
        mmap_chunks = [mmap.get_chunk(i) for i in range(mmap.chunk_count)]
        mmap.close()

        self.assertEqual(
            len(py_chunks),
            len(mmap_chunks),
            "chunk count mismatch",
        )
        for i, (pc, mc) in enumerate(zip(py_chunks, mmap_chunks)):
            self.assertEqual(pc, mc, "chunk %d mismatch" % i)
        self.assertEqual(
            b"".join(py_chunks),
            b"".join(mmap_chunks),
            "total bytes mismatch",
        )

    def test_fixed_lines(self) -> None:
        self._compare(b"aaa\nbbb\nccc\nddd\neee\n", 6)

    def test_single_record(self) -> None:
        self._compare(b"hello\nworld\n", 1024)

    def test_many_records(self) -> None:
        lines = [("line_%08d\n" % i).encode() for i in range(500)]
        self._compare(b"".join(lines), 4096)

    def test_no_delimiter(self) -> None:
        self._compare(b"this_has_no_delimiters_at_all", 10)

    def test_only_newlines(self) -> None:
        self._compare(b"\n\n\n\n\n", 1)

    def test_consecutive_delimiters(self) -> None:
        self._compare(b"a\n\n\nb\n\nc\n", 4)

    def test_no_trailing_newline(self) -> None:
        self._compare(b"line1\nline2\nline3", 4)

    def test_one_byte_file(self) -> None:
        self._compare(b"x", 1024)

    def test_one_byte_newline(self) -> None:
        self._compare(b"\n", 1024)

    def test_nul_bytes(self) -> None:
        self._compare(b"prefix\x00suffix\nmore\x00data\n", 100)


class TestProtocol(TestFileFixture, unittest.TestCase):
    def test_python_satisfies_protocol(self) -> None:
        self.assertIsInstance(PythonChunkProvider(), ByteChunkProvider)

    def test_mmap_satisfies_protocol(self) -> None:
        from native_io.mmap_provider import MmapChunkProvider

        if MMAP_AVAILABLE:
            self.assertIsInstance(MmapChunkProvider(), ByteChunkProvider)
        else:
            self.skipTest("mmap not available")

    def test_protocol_attributes(self) -> None:
        p = PythonChunkProvider()
        self.assertEqual(p.backend, "python")
        self.assertFalse(p.zero_copy)
        self.assertEqual(p.chunk_count, 0)

    def test_mmap_protocol_attributes(self) -> None:
        from native_io.mmap_provider import MmapChunkProvider

        if not MMAP_AVAILABLE:
            self.skipTest("mmap not available")
        p = MmapChunkProvider()
        self.assertEqual(p.backend, "mmap")
        self.assertTrue(p.zero_copy)
        self.assertEqual(p.chunk_count, 0)


class TestDiscovery(unittest.TestCase):
    def test_discover_returns_structured_result(self) -> None:
        info = discover_mmap_provider()
        self.assertIn(info.provider, ("mmap",))
        self.assertIsInstance(info.available, bool)
        self.assertIsInstance(info.zero_copy, bool)
        result = info.to_dict()
        self.assertIn("provider", result)
        self.assertIn("available", result)
        self.assertIn("zero_copy", result)
        self.assertIn("platform", result)
        self.assertIn("reason", result)

    def test_discover_does_not_raise(self) -> None:
        discover_mmap_provider()

    def test_create_provider_python(self) -> None:
        p = create_provider("python")
        self.assertIsInstance(p, PythonChunkProvider)
        self.assertEqual(p.backend, "python")

    def test_create_provider_auto(self) -> None:
        p = create_provider("auto")
        self.assertIsInstance(p, ByteChunkProvider)

    def test_create_provider_invalid(self) -> None:
        with self.assertRaises(ValueError):
            create_provider("nonexistent")

    def test_select_python(self) -> None:
        p = select_provider(mode="python")
        self.assertEqual(p.backend, "python")

    def test_select_auto_empty(self) -> None:
        p = select_provider(file_size=0, mode="auto")
        self.assertEqual(p.backend, "python")

    def test_select_explicit(self) -> None:
        p = select_provider(mode="explicit", preferred_provider="python")
        self.assertEqual(p.backend, "python")

    def test_select_invalid_mode(self) -> None:
        with self.assertRaises(ValueError):
            select_provider(mode="invalid")


class TestShadowCompare(TestFileFixture, unittest.TestCase):
    def test_shadow_on_small_file(self) -> None:
        content = b"line1\nline2\nline3\nline4\nline5\n"
        path = self._make_file("shadow.txt", content)
        result = shadow_compare(path, chunk_size=4)
        d = result.to_dict()
        self.assertIn("baseline_backend", d)
        self.assertIn("native_backend", d)
        self.assertIn("baseline_elapsed_ms", d)
        if result.native_available:
            self.assertTrue(result.results_match)


class TestFullCoverage(TestFileFixture, unittest.TestCase):
    """Verify chunks exactly partition the entire file."""

    def test_python_full_coverage(self) -> None:
        content = self._make_large_content(1000)
        path = self._make_file("coverage.txt", content)
        doc = PythonChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=1024)
        covered = bytearray()
        for i in range(doc.chunk_count):
            c = doc.get_chunk(i)
            self.assertGreater(len(c), 0, "chunk %d is empty" % i)
            covered.extend(c)
        self.assertEqual(bytes(covered), content)
        doc.close()

    def test_mmap_full_coverage(self) -> None:
        from native_io.mmap_provider import MmapChunkProvider

        if not MMAP_AVAILABLE:
            self.skipTest("mmap not available")
        content = self._make_large_content(1000)
        path = self._make_file("coverage.txt", content)
        doc = MmapChunkProvider()
        doc.open(path)
        doc.scan(chunk_size=1024)
        covered = bytearray()
        for i in range(doc.chunk_count):
            c = doc.get_chunk(i)
            self.assertGreater(len(c), 0, "chunk %d is empty" % i)
            covered.extend(c)
        self.assertEqual(bytes(covered), content)
        doc.close()

    @staticmethod
    def _make_large_content(lines: int) -> bytes:
        parts = [
            ("line_%08d,data_%08d,value_%08d\n" % (i, i, i)).encode()
            for i in range(lines)
        ]
        return b"".join(parts)


if __name__ == "__main__":
    unittest.main(verbosity=2)
