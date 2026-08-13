#!/usr/bin/env python3
"""Small independent ctypes consumer for the Linux ABI conformance gate."""

from __future__ import annotations

import argparse
import ctypes
from pathlib import Path


class CChunkView(ctypes.Structure):
    _fields_ = [("data", ctypes.POINTER(ctypes.c_ubyte)), ("len", ctypes.c_size_t)]


FNV_OFFSET = 14695981039346656037
FNV_PRIME = 1099511628211


def fnv1a(chunks: list[bytes]) -> int:
    value = FNV_OFFSET
    for chunk in chunks:
        for byte in chunk:
            value ^= byte
            value = (value * FNV_PRIME) & ((1 << 64) - 1)
    return value


def configure(library: Path) -> ctypes.CDLL:
    lib = ctypes.CDLL(str(library))
    lib.mmap_engine_abi_version.restype = ctypes.c_uint32
    lib.mmap_engine_capabilities.restype = ctypes.c_uint32
    lib.mmap_engine_last_error.restype = ctypes.c_char_p
    lib.mmap_engine_open.argtypes = [ctypes.c_char_p]
    lib.mmap_engine_open.restype = ctypes.c_void_p
    lib.mmap_engine_partition_records.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_ubyte]
    lib.mmap_engine_partition_records.restype = ctypes.c_size_t
    lib.mmap_engine_get_chunk.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(CChunkView)]
    lib.mmap_engine_get_chunk.restype = ctypes.c_int32
    lib.mmap_engine_free.argtypes = [ctypes.c_void_p]
    lib.mmap_engine_free.restype = None
    return lib


def error_text(lib: ctypes.CDLL) -> str:
    raw = lib.mmap_engine_last_error()
    return "" if raw is None else raw.decode("utf-8")


def capture(lib: ctypes.CDLL, handle: int, source: bytes) -> tuple[list[bytes], int]:
    count = lib.mmap_engine_partition_records(handle, 4, 0x0A)
    if count == 0:
        raise AssertionError("unexpected partition count")
    chunks: list[bytes] = []
    offset = 0
    for index in range(count):
        view = CChunkView()
        if lib.mmap_engine_get_chunk(handle, index, ctypes.byref(view)) != 0:
            raise AssertionError("could not retrieve partition")
        chunk = ctypes.string_at(view.data, view.len)
        if not chunk or chunk != source[offset : offset + view.len]:
            raise AssertionError("partition bytes differ from fixture")
        if index + 1 < count and not chunk.endswith(b"\n"):
            raise AssertionError("non-final partition splits a record")
        chunks.append(chunk)
        offset += view.len
    if offset != len(source):
        raise AssertionError("partition plan does not reconstruct the fixture")
    return chunks, fnv1a(chunks)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--library", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--expected", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    if ctypes.sizeof(CChunkView) != 16 or CChunkView.data.offset != 0 or CChunkView.len.offset != 8:
        raise AssertionError("CChunkView layout mismatch")
    lib = configure(args.library.resolve())
    if lib.mmap_engine_abi_version() != 0x00010003 or lib.mmap_engine_capabilities() != 63:
        raise AssertionError("ABI discovery mismatch")

    source = args.fixture.read_bytes()
    handle = lib.mmap_engine_open(str(args.fixture).encode("utf-8"))
    if not handle:
        raise AssertionError("mmap_engine_open failed for UTF-8 path")
    first, digest = capture(lib, handle, source)
    second, repeat_digest = capture(lib, handle, source)
    if first != second or digest != repeat_digest:
        raise AssertionError("partition plan is not deterministic")
    if lib.mmap_engine_partition_records(handle, 0, 0x0A) != 0:
        raise AssertionError("N=0 unexpectedly succeeded")
    n0_error = error_text(lib)
    if n0_error != "requested_partitions must be > 0":
        raise AssertionError(f"N=0 error contract mismatch: {n0_error!r}")
    lib.mmap_engine_free(handle)

    record_count = source.count(b"\n") + int(bool(source) and not source.endswith(b"\n"))
    result = (
        f"abi_version=65539;capabilities=63;partition_count={len(first)};"
        f"partition_lengths={','.join(str(len(chunk)) for chunk in first)};"
        f"total_length={len(source)};record_count={record_count};"
        f"fnv1a64={digest:016x};deterministic=1;n0_error={n0_error};"
        f"chunk_view_size={ctypes.sizeof(CChunkView)};"
        f"chunk_view_data_offset={CChunkView.data.offset};"
        f"chunk_view_len_offset={CChunkView.len.offset}"
    )
    expected = args.expected.read_text(encoding="utf-8").strip()
    if result != expected:
        raise AssertionError(f"canonical result mismatch\nexpected: {expected}\nactual:   {result}")
    args.output.write_text(result + "\n", encoding="utf-8")
    print("PASS: Python conformance consumer")


if __name__ == "__main__":
    main()
