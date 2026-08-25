#!/usr/bin/env python3
"""Independent, bounded semantic checks for the public C ABI.

This is deliberately a plain-Python reference rather than a binding or a
translation of the Rust implementation.  It compares observable byte chunks
returned by the release cdylib with simple reference planners over a small,
deterministic corpus.  It has no external dependencies and creates fixtures
only in the system temporary directory.
"""

from __future__ import annotations

import ctypes
import os
import platform
import tempfile
from pathlib import Path
from time import perf_counter


class ChunkView(ctypes.Structure):
    _fields_ = [("data", ctypes.POINTER(ctypes.c_ubyte)), ("len", ctypes.c_size_t)]


def delimited_reference(data: bytes, size: int, delimiter: int) -> list[bytes]:
    """Return chunks ending after the next delimiter at/after each target."""
    step = max(size, 1)
    chunks: list[bytes] = []
    start = 0
    while start < len(data):
        end = min(start + step, len(data))
        if end < len(data):
            found = data.find(bytes((delimiter,)), end)
            end = len(data) if found == -1 else found + 1
        chunks.append(data[start:end])
        start = end
    return chunks


def pattern_reference(data: bytes, size: int, delimiter: bytes) -> list[bytes]:
    """Return chunks ending after the next complete pattern at each target."""
    assert delimiter
    step = max(size, 1)
    chunks: list[bytes] = []
    start = 0
    while start < len(data):
        end = min(start + step, len(data))
        if end < len(data):
            found = data.find(delimiter, end)
            end = len(data) if found == -1 else found + len(delimiter)
        chunks.append(data[start:end])
        start = end
    return chunks


def fixed_reference(data: bytes, size: int) -> list[bytes]:
    step = max(size, 1)
    return [data[start : start + step] for start in range(0, len(data), step)]


def partition_reference(data: bytes, count: int, delimiter: int) -> list[bytes]:
    """Use independent absolute targets and forward byte searches."""
    if not data or count == 0:
        return []
    count = min(count, len(data))
    boundaries: list[int] = []
    last = 0
    for index in range(1, count):
        target = len(data) * index // count
        if target <= last:
            continue
        if data[target - 1] == delimiter:
            last = target
            boundaries.append(last)
            continue
        found = data.find(bytes((delimiter,)), target)
        if found == -1:
            boundaries.append(len(data))
            break
        last = found + 1
        boundaries.append(last)

    ends = [boundary for boundary in boundaries if boundary > 0]
    if not ends or ends[-1] != len(data):
        ends.append(len(data))
    chunks: list[bytes] = []
    start = 0
    for end in ends:
        if end > start:
            chunks.append(data[start:end])
        start = end
    return chunks


def library_path(root: Path) -> Path:
    name = {
        "Windows": "mmap_chunker_core.dll",
        "Darwin": "libmmap_chunker_core.dylib",
    }.get(platform.system(), "libmmap_chunker_core.so")
    return root / "target" / "release" / name


def configure(path: Path) -> ctypes.CDLL:
    lib = ctypes.CDLL(str(path))
    lib.mmap_engine_open.argtypes = [ctypes.c_char_p]
    lib.mmap_engine_open.restype = ctypes.c_void_p
    lib.mmap_engine_free.argtypes = [ctypes.c_void_p]
    lib.mmap_engine_free.restype = None
    lib.mmap_engine_get_chunk.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ChunkView)]
    lib.mmap_engine_get_chunk.restype = ctypes.c_int32
    for name in ("mmap_engine_scan_chunks_ex", "mmap_engine_partition_records"):
        function = getattr(lib, name)
        function.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_ubyte]
        function.restype = ctypes.c_size_t
    lib.mmap_engine_scan_fixed.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    lib.mmap_engine_scan_fixed.restype = ctypes.c_size_t
    lib.mmap_engine_scan_chunks_pattern.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_ubyte),
        ctypes.c_size_t,
    ]
    lib.mmap_engine_scan_chunks_pattern.restype = ctypes.c_size_t
    return lib


def ffi_chunks(lib: ctypes.CDLL, path: Path, mode: str, value: int | bytes) -> list[bytes]:
    handle = lib.mmap_engine_open(os.fsencode(path))
    assert handle, f"mmap_engine_open failed for {path}"
    try:
        if mode == "single":
            size, delimiter = value  # type: ignore[misc]
            count = lib.mmap_engine_scan_chunks_ex(handle, size, delimiter)
        elif mode == "pattern":
            size, delimiter = value  # type: ignore[misc]
            storage = (ctypes.c_ubyte * len(delimiter)).from_buffer_copy(delimiter)
            count = lib.mmap_engine_scan_chunks_pattern(handle, size, storage, len(delimiter))
        elif mode == "fixed":
            count = lib.mmap_engine_scan_fixed(handle, value)  # type: ignore[arg-type]
        else:
            partitions, delimiter = value  # type: ignore[misc]
            count = lib.mmap_engine_partition_records(handle, partitions, delimiter)

        chunks = []
        for index in range(count):
            view = ChunkView()
            assert lib.mmap_engine_get_chunk(handle, index, ctypes.byref(view)) == 0
            chunks.append(ctypes.string_at(view.data, view.len))
        return chunks
    finally:
        lib.mmap_engine_free(handle)


def assert_equal(name: str, expected: list[bytes], actual: list[bytes]) -> None:
    if expected != actual:
        raise AssertionError(f"{name}: expected {expected!r}, got {actual!r}")


def mismatch_proof() -> None:
    """Prove the comparator rejects a deliberately corrupted expected result."""
    try:
        assert_equal("controlled mismatch", [b"wrong"], [b"right"])
    except AssertionError:
        return
    raise AssertionError("controlled mismatch was not detected")


def generated_cases() -> list[bytes]:
    state = 0x5059_5448_4F4E_0001
    cases = []
    for index in range(48):
        length = index * 3 % 97
        data = bytearray()
        for _ in range(length):
            state = (state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
            data.append(state >> 56)
        if data:
            data[index % len(data)] = 0x0A
        cases.append(bytes(data))
    return cases


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    lib_path = library_path(root)
    if not lib_path.is_file():
        raise SystemExit(f"missing release cdylib: {lib_path}; run cargo build --release first")
    lib = configure(lib_path)
    mismatch_proof()

    cases = [
        ("empty", b""),
        ("one_byte", b"x"),
        ("final_record", b"a\nb\nc"),
        ("adjacent_delimiters", b"a\n\nb\n"),
        ("crlf", b"a\r\nb\r\nc"),
        ("no_delimiter", b"very-long-record-without-a-terminator"),
        ("long_record", b"x" * 4097 + b"\nshort\n"),
    ] + [(f"generated_{index:02d}", data) for index, data in enumerate(generated_cases())]

    checks = 0
    started = perf_counter()
    with tempfile.TemporaryDirectory(prefix="mmap_chunker_python_parity_") as temp:
        directory = Path(temp)
        for name, data in cases:
            path = directory / f"{name}.bin"
            path.write_bytes(data)
            for size in (0, 1, 4, 17):
                assert_equal(name + ":single", delimited_reference(data, size, 0x0A), ffi_chunks(lib, path, "single", (size, 0x0A)))
                assert_equal(name + ":pattern", pattern_reference(data, size, b"\r\n"), ffi_chunks(lib, path, "pattern", (size, b"\r\n")))
                assert_equal(name + ":fixed", fixed_reference(data, size), ffi_chunks(lib, path, "fixed", size))
                checks += 3
            for partitions in (0, 1, 2, 5, 17):
                assert_equal(name + ":partition", partition_reference(data, partitions, 0x0A), ffi_chunks(lib, path, "partition", (partitions, 0x0A)))
                checks += 1
    elapsed = perf_counter() - started
    print(f"PASS: Python C-ABI parity: {checks} checks across {len(cases)} cases in {elapsed:.3f}s")
    print("PASS: controlled mismatch detected")


if __name__ == "__main__":
    main()
