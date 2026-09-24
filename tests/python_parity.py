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
from typing import Any


class ChunkView(ctypes.Structure):
    _fields_ = [("data", ctypes.POINTER(ctypes.c_ubyte)), ("len", ctypes.c_size_t)]


class PartitionRange(ctypes.Structure):
    _fields_ = [("start", ctypes.c_size_t), ("end", ctypes.c_size_t)]


class FrameSpec(ctypes.Structure):
    _fields_ = [
        ("kind", ctypes.c_uint32),
        ("delimiter", ctypes.POINTER(ctypes.c_ubyte)),
        ("delimiter_len", ctypes.c_size_t),
        ("record_bytes", ctypes.c_size_t),
        ("prefix_bytes", ctypes.c_uint32),
        ("prefix_little_endian", ctypes.c_uint32),
        ("length_includes_prefix", ctypes.c_uint32),
    ]


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
    boundaries: list[int] = []
    last = 0
    for index in range(1, count):
        target = len(data) * index // count
        if target <= last:
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


def partition_pattern_reference(
    data: bytes, count: int, delimiter: bytes
) -> list[bytes]:
    """Record-aligned partitions using a multi-byte delimiter pattern."""
    assert delimiter
    if not data or count == 0:
        return []
    boundaries: list[int] = []
    last = 0
    for index in range(1, count):
        target = len(data) * index // count
        if target <= last:
            continue
        found = data.find(delimiter, target)
        if found == -1:
            boundaries.append(len(data))
            break
        last = found + len(delimiter)
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
    lib.mmap_engine_get_chunk.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(ChunkView),
    ]
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
    lib.mmap_engine_partition_records_pattern.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_ubyte),
        ctypes.c_size_t,
    ]
    lib.mmap_engine_partition_records_pattern.restype = ctypes.c_size_t
    lib.mmap_engine_plan_partition_ranges.argtypes = [
        ctypes.c_char_p,
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_ubyte),
        ctypes.c_size_t,
        ctypes.c_uint32,
        ctypes.c_size_t,
        ctypes.POINTER(PartitionRange),
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    lib.mmap_engine_plan_partition_ranges.restype = ctypes.c_int32
    lib.mmap_engine_plan_partition_ranges_framed.argtypes = [
        ctypes.c_char_p,
        ctypes.c_size_t,
        ctypes.POINTER(FrameSpec),
        ctypes.c_uint32,
        ctypes.c_size_t,
        ctypes.POINTER(PartitionRange),
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    lib.mmap_engine_plan_partition_ranges_framed.restype = ctypes.c_int32
    return lib


def ffi_chunks(lib: ctypes.CDLL, path: Path, mode: str, value: Any) -> list[bytes]:
    handle = lib.mmap_engine_open(os.fsencode(path))
    assert handle, f"mmap_engine_open failed for {path}"
    try:
        if mode == "single":
            size, delimiter = value
            count = lib.mmap_engine_scan_chunks_ex(handle, size, delimiter)
        elif mode == "pattern":
            size, delimiter = value
            storage = (ctypes.c_ubyte * len(delimiter)).from_buffer_copy(delimiter)
            count = lib.mmap_engine_scan_chunks_pattern(
                handle, size, storage, len(delimiter)
            )
        elif mode == "fixed":
            count = lib.mmap_engine_scan_fixed(handle, value)
        elif mode == "partition_pattern":
            partitions, delimiter = value
            storage = (ctypes.c_ubyte * len(delimiter)).from_buffer_copy(delimiter)
            count = lib.mmap_engine_partition_records_pattern(
                handle, partitions, storage, len(delimiter)
            )
        else:
            partitions, delimiter = value
            count = lib.mmap_engine_partition_records(handle, partitions, delimiter)

        chunks = []
        for index in range(count):
            view = ChunkView()
            assert lib.mmap_engine_get_chunk(handle, index, ctypes.byref(view)) == 0
            chunks.append(ctypes.string_at(view.data, view.len))
        return chunks
    finally:
        lib.mmap_engine_free(handle)


def assert_equal(name: str, expected: object, actual: object) -> None:
    if expected != actual:
        raise AssertionError(f"{name}: expected {expected!r}, got {actual!r}")


def ffi_plan_ranges(
    lib: ctypes.CDLL,
    path: Path,
    partitions: int,
    delimiter: bytes,
    source_mode: int,
    window: int,
) -> list[tuple[int, int]]:
    """Call the two-phase C ABI planner and return its ranges."""
    storage = (ctypes.c_ubyte * len(delimiter)).from_buffer_copy(delimiter)
    needed = ctypes.c_size_t(0)
    result = lib.mmap_engine_plan_partition_ranges(
        os.fsencode(path),
        partitions,
        storage,
        len(delimiter),
        source_mode,
        window,
        None,
        0,
        ctypes.byref(needed),
    )
    if result == 0:
        assert needed.value == 0, "empty result must report zero ranges"
        return []
    assert result == -2, f"query should report capacity, got {result}"
    ranges = (PartitionRange * needed.value)()
    result = lib.mmap_engine_plan_partition_ranges(
        os.fsencode(path),
        partitions,
        storage,
        len(delimiter),
        source_mode,
        window,
        ranges,
        needed.value,
        ctypes.byref(needed),
    )
    assert result == 0, f"planning failed with {result}"
    return [(ranges[index].start, ranges[index].end) for index in range(needed.value)]


def ranges_to_chunks(data: bytes, ranges: list[tuple[int, int]]) -> list[bytes]:
    """Validate exact coverage and materialize one bytes object per range."""
    chunks: list[bytes] = []
    cursor = 0
    for start, end in ranges:
        assert start == cursor, "gap or overlap in planner output"
        assert end > start, "empty range in planner output"
        chunks.append(data[start:end])
        cursor = end
    assert cursor == len(data), "incomplete planner coverage"
    return chunks


def ffi_plan_ranges_framed(
    lib: ctypes.CDLL,
    path: Path,
    partitions: int,
    frame: FrameSpec,
    source_mode: int,
    window: int,
) -> list[tuple[int, int]]:
    """Call the framed two-phase C ABI planner and return its ranges."""
    needed = ctypes.c_size_t(0)
    result = lib.mmap_engine_plan_partition_ranges_framed(
        os.fsencode(path),
        partitions,
        ctypes.byref(frame),
        source_mode,
        window,
        None,
        0,
        ctypes.byref(needed),
    )
    if result == 0:
        assert needed.value == 0, "empty result must report zero ranges"
        return []
    assert result == -2, f"query should report capacity, got {result}"
    ranges = (PartitionRange * needed.value)()
    result = lib.mmap_engine_plan_partition_ranges_framed(
        os.fsencode(path),
        partitions,
        ctypes.byref(frame),
        source_mode,
        window,
        ranges,
        needed.value,
        ctypes.byref(needed),
    )
    assert result == 0, f"framed planning failed with {result}"
    return [(ranges[index].start, ranges[index].end) for index in range(needed.value)]


def fixed_width_partition_reference(
    data: bytes, count: int, record_bytes: int
) -> list[tuple[int, int]]:
    """Smallest record boundary strictly after each ideal byte target."""
    if not data or count == 0:
        return []
    if count == 1:
        return [(0, len(data))]
    n = min(count, len(data))
    boundaries: list[int] = []
    last = 0
    for partition in range(1, n):
        target = len(data) * partition // n
        if target <= last:
            continue
        boundary = min(((target // record_bytes) + 1) * record_bytes, len(data))
        boundaries.append(boundary)
        last = boundary
    ranges: list[tuple[int, int]] = []
    previous = 0
    for boundary in boundaries:
        if boundary > previous:
            ranges.append((previous, boundary))
        previous = boundary
    if previous < len(data):
        ranges.append((previous, len(data)))
    return ranges


def length_prefixed_partition_reference(
    data: bytes,
    count: int,
    prefix_bytes: int,
    little_endian: bool,
    length_includes_prefix: bool,
) -> list[tuple[int, int]]:
    """Record ends parsed from length prefixes, then target-based cuts."""
    if not data or count == 0:
        return []
    ends: list[int] = []
    cursor = 0
    while cursor < len(data):
        if cursor + prefix_bytes > len(data):
            raise AssertionError("truncated length prefix in fixture")
        prefix = int.from_bytes(
            data[cursor : cursor + prefix_bytes],
            "little" if little_endian else "big",
        )
        length = prefix if length_includes_prefix else prefix + prefix_bytes
        cursor += length
        if cursor > len(data):
            raise AssertionError("record exceeds fixture")
        ends.append(cursor)
    if count == 1:
        return [(0, len(data))]
    n = min(count, len(data))
    boundaries: list[int] = []
    last = 0
    for partition in range(1, n):
        target = len(data) * partition // n
        if target <= last:
            continue
        boundary = next((end for end in ends if end > target), len(data))
        boundaries.append(boundary)
        last = boundary
    ranges: list[tuple[int, int]] = []
    previous = 0
    for boundary in boundaries:
        if boundary > previous:
            ranges.append((previous, boundary))
        previous = boundary
    if previous < len(data):
        ranges.append((previous, len(data)))
    return ranges


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
            state = (state * 6364136223846793005 + 1442695040888963407) & (
                (1 << 64) - 1
            )
            data.append(state >> 56)
        if data:
            data[index % len(data)] = 0x0A
        cases.append(bytes(data))
    return cases


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    lib_path = library_path(root)
    if not lib_path.is_file():
        raise SystemExit(
            f"missing release cdylib: {lib_path}; run cargo build --release first"
        )
    lib = configure(lib_path)
    assert lib.mmap_engine_abi_version() >= 0x00010006, "unexpected ABI version"
    assert lib.mmap_engine_capabilities() & (1 << 7), "windowed planning cap missing"
    assert lib.mmap_engine_capabilities() & (1 << 8), "framing strategy cap missing"
    mismatch_proof()

    cases = [
        ("empty", b""),
        ("one_byte", b"x"),
        ("final_record", b"a\nb\nc"),
        ("adjacent_delimiters", b"a\n\nb\n"),
        ("crlf", b"a\r\nb\r\nc"),
        ("no_delimiter", b"very-long-record-without-a-terminator"),
        ("long_record", b"x" * 4097 + b"\nshort\n"),
    ] + [
        (f"generated_{index:02d}", data) for index, data in enumerate(generated_cases())
    ]

    checks = 0
    started = perf_counter()
    with tempfile.TemporaryDirectory(prefix="mmap_chunker_python_parity_") as temp:
        directory = Path(temp)
        for name, data in cases:
            path = directory / f"{name}.bin"
            path.write_bytes(data)
            for size in (0, 1, 4, 17):
                assert_equal(
                    name + ":single",
                    delimited_reference(data, size, 0x0A),
                    ffi_chunks(lib, path, "single", (size, 0x0A)),
                )
                assert_equal(
                    name + ":pattern",
                    pattern_reference(data, size, b"\r\n"),
                    ffi_chunks(lib, path, "pattern", (size, b"\r\n")),
                )
                assert_equal(
                    name + ":fixed",
                    fixed_reference(data, size),
                    ffi_chunks(lib, path, "fixed", size),
                )
                checks += 3
            for partitions in (0, 1, 2, 5, 17):
                assert_equal(
                    name + ":partition",
                    partition_reference(data, partitions, 0x0A),
                    ffi_chunks(lib, path, "partition", (partitions, 0x0A)),
                )
                checks += 1
                for pattern in (b"\r\n", b"\r\n\r\n"):
                    reference = partition_pattern_reference(data, partitions, pattern)
                    assert_equal(
                        name + ":partition_pattern",
                        reference,
                        ffi_chunks(
                            lib, path, "partition_pattern", (partitions, pattern)
                        ),
                    )
                    checks += 1
                    if partitions == 0:
                        continue
                    for source_mode, window in ((0, 0), (1, 65536), (2, 0)):
                        ranges = ffi_plan_ranges(
                            lib, path, partitions, pattern, source_mode, window
                        )
                        assert_equal(
                            name + ":plan_mode_%d" % source_mode,
                            reference,
                            ranges_to_chunks(data, ranges),
                        )
                        checks += 1
        zero_partitions = lib.mmap_engine_plan_partition_ranges(
            os.fsencode(directory / "empty.bin"),
            0,
            (ctypes.c_ubyte * 1)(0x0A),
            1,
            0,
            0,
            None,
            0,
            ctypes.byref(ctypes.c_size_t(0)),
        )
        assert zero_partitions == -1, "zero partitions must be rejected"
        checks += 1

        # Framed planning: fixed-width and length-prefixed records.
        fixed_data = bytes((index * 7) % 256 for index in range(4096))
        fixed_path = directory / "framed_fixed.bin"
        fixed_path.write_bytes(fixed_data)
        fixed_frame = FrameSpec(
            kind=1,
            delimiter=None,
            delimiter_len=0,
            record_bytes=64,
            prefix_bytes=0,
            prefix_little_endian=0,
            length_includes_prefix=0,
        )
        for partitions in (1, 2, 5, 17, 64):
            reference = fixed_width_partition_reference(fixed_data, partitions, 64)
            for source_mode, window in ((0, 0), (1, 65536), (2, 0)):
                ranges = ffi_plan_ranges_framed(
                    lib, fixed_path, partitions, fixed_frame, source_mode, window
                )
                ranges_to_chunks(fixed_data, ranges)
                assert_equal("framed_fixed_mode_%d" % source_mode, reference, ranges)
                checks += 1

        for prefix_bytes, little_endian, includes_prefix in (
            (1, True, False),
            (2, False, False),
            (4, True, True),
        ):
            payloads = [
                bytes([65 + (index % 26)]) * (index % 19) for index in range(300)
            ]
            framed = bytearray()
            for payload in payloads:
                length = (
                    len(payload) + prefix_bytes if includes_prefix else len(payload)
                )
                framed += length.to_bytes(
                    prefix_bytes, "little" if little_endian else "big"
                )
                framed += payload
            framed_data = bytes(framed)
            framed_path = directory / (
                "framed_length_%d_%s_%s.bin"
                % (prefix_bytes, little_endian, includes_prefix)
            )
            framed_path.write_bytes(framed_data)
            frame = FrameSpec(
                kind=2,
                delimiter=None,
                delimiter_len=0,
                record_bytes=0,
                prefix_bytes=prefix_bytes,
                prefix_little_endian=1 if little_endian else 0,
                length_includes_prefix=1 if includes_prefix else 0,
            )
            for partitions in (1, 3, 7, 32):
                reference = length_prefixed_partition_reference(
                    framed_data,
                    partitions,
                    prefix_bytes,
                    little_endian,
                    includes_prefix,
                )
                ranges = ffi_plan_ranges_framed(
                    lib, framed_path, partitions, frame, 1, 65536
                )
                ranges_to_chunks(framed_data, ranges)
                assert_equal(
                    "framed_length_%d_%s_%s"
                    % (prefix_bytes, little_endian, includes_prefix),
                    reference,
                    ranges,
                )
                checks += 1

        # Invalid framing specs must be rejected.
        bad_frame = FrameSpec(
            kind=1,
            delimiter=None,
            delimiter_len=0,
            record_bytes=0,
            prefix_bytes=0,
            prefix_little_endian=0,
            length_includes_prefix=0,
        )
        assert (
            lib.mmap_engine_plan_partition_ranges_framed(
                os.fsencode(fixed_path),
                4,
                ctypes.byref(bad_frame),
                0,
                0,
                None,
                0,
                ctypes.byref(ctypes.c_size_t(0)),
            )
            == -1
        ), "zero record_bytes must be rejected"
        checks += 1
    elapsed = perf_counter() - started
    print(
        f"PASS: Python C-ABI parity: {checks} checks across {len(cases)} cases in {elapsed:.3f}s"
    )
    print("PASS: controlled mismatch detected")


if __name__ == "__main__":
    main()
