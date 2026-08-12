#!/usr/bin/env python3
"""Bounded external-consumer proof for record-aligned multiprocessing.

This is deliberately a reference integration rather than a Python binding.
The planner uses only the released/public C ABI through stdlib ctypes.  Each
worker then opens the original JSONL file independently and processes exactly
one derived byte range.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import multiprocessing as mp
import os
import platform
import sys
import tempfile
import time
from pathlib import Path
from typing import Iterable


class ChunkView(ctypes.Structure):
    _fields_ = [("data", ctypes.POINTER(ctypes.c_ubyte)), ("len", ctypes.c_size_t)]


def library_name() -> str:
    if os.name == "nt":
        return "mmap_chunker_core.dll"
    if platform.system() == "Darwin":
        return "libmmap_chunker_core.dylib"
    return "libmmap_chunker_core.so"


def load_library(path: Path) -> ctypes.CDLL:
    if os.name == "nt":
        add_dll_directory = getattr(os, "add_dll_directory", None)
        if add_dll_directory is not None:
            add_dll_directory(str(path.parent))

    lib = ctypes.CDLL(str(path))
    lib.mmap_engine_abi_version.argtypes = []
    lib.mmap_engine_abi_version.restype = ctypes.c_uint32
    lib.mmap_engine_capabilities.argtypes = []
    lib.mmap_engine_capabilities.restype = ctypes.c_uint32
    lib.mmap_engine_last_error.argtypes = []
    lib.mmap_engine_last_error.restype = ctypes.c_char_p
    lib.mmap_engine_open.argtypes = [ctypes.c_char_p]
    lib.mmap_engine_open.restype = ctypes.c_void_p
    lib.mmap_engine_partition_records.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_ubyte,
    ]
    lib.mmap_engine_partition_records.restype = ctypes.c_size_t
    lib.mmap_engine_get_chunk.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(ChunkView),
    ]
    lib.mmap_engine_get_chunk.restype = ctypes.c_int32
    lib.mmap_engine_free.argtypes = [ctypes.c_void_p]
    lib.mmap_engine_free.restype = None
    return lib


def last_error(lib: ctypes.CDLL) -> str:
    value = lib.mmap_engine_last_error()
    return value.decode("utf-8", "replace") if value else "unknown error"


def plan_ranges(lib: ctypes.CDLL, path: Path, requested_workers: int) -> dict:
    started = time.perf_counter()
    handle = lib.mmap_engine_open(os.fsencode(path))
    if not handle:
        raise RuntimeError(f"mmap_engine_open failed: {last_error(lib)}")

    try:
        plan_started = time.perf_counter()
        count = lib.mmap_engine_partition_records(handle, requested_workers, 0x0A)
        planning_ms = (time.perf_counter() - plan_started) * 1000
        if not count:
            raise RuntimeError(f"partition planning failed: {last_error(lib)}")

        lengths: list[int] = []
        for index in range(count):
            view = ChunkView()
            if lib.mmap_engine_get_chunk(handle, index, ctypes.byref(view)) != 0:
                raise RuntimeError(f"get_chunk({index}) failed: {last_error(lib)}")
            lengths.append(int(view.len))
    finally:
        lib.mmap_engine_free(handle)

    ranges = []
    offset = 0
    for length in lengths:
        ranges.append((offset, length))
        offset += length

    file_size = path.stat().st_size
    if offset != file_size:
        raise AssertionError(f"partition coverage {offset} != file size {file_size}")

    return {
        "requested_workers": requested_workers,
        "actual_partitions": len(ranges),
        "lengths": lengths,
        "ranges": ranges,
        "file_size": file_size,
        "partition_planning_ms": planning_ms,
        "planning_total_ms": (time.perf_counter() - started) * 1000,
    }


def generate_jsonl(path: Path, records: int, payload_bytes: int) -> None:
    payload = "x" * payload_bytes
    with path.open("wb") as output:
        for record_id in range(records):
            record = {
                "id": record_id,
                "value": (record_id * 1_000_003) % 997_651,
                "group": record_id % 17,
                "payload": payload,
            }
            output.write(json.dumps(record, separators=(",", ":")).encode("utf-8"))
            output.write(b"\n")


def summarize_lines(lines: Iterable[bytes]) -> tuple[int, int, int]:
    count = 0
    value_sum = 0
    byte_count = 0
    for line in lines:
        if not line:
            continue
        byte_count += len(line)
        value_sum += int(json.loads(line)["value"])
        count += 1
    return count, value_sum, byte_count


def single_process_reference(path: Path) -> dict:
    started = time.perf_counter()
    with path.open("rb") as input_file:
        count, value_sum, byte_count = summarize_lines(input_file)
    return {
        "record_count": count,
        "value_sum": value_sum,
        "bytes_processed": byte_count,
        "wall_ms": (time.perf_counter() - started) * 1000,
    }


def process_range(task: tuple[str, int, int]) -> dict:
    path_string, offset, length = task
    started = time.perf_counter()
    with open(path_string, "rb") as input_file:
        input_file.seek(offset)
        payload = input_file.read(length)
    count, value_sum, byte_count = summarize_lines(payload.splitlines(keepends=True))
    return {
        "record_count": count,
        "value_sum": value_sum,
        "bytes_processed": byte_count,
        "range_bytes": len(payload),
        "worker_ms": (time.perf_counter() - started) * 1000,
    }


def multiprocessing_run(path: Path, ranges: list[tuple[int, int]], workers: int) -> dict:
    context = mp.get_context("spawn")
    tasks = [(str(path), offset, length) for offset, length in ranges]

    startup_started = time.perf_counter()
    pool = context.Pool(processes=workers)
    startup_ms = (time.perf_counter() - startup_started) * 1000
    try:
        processing_started = time.perf_counter()
        results = pool.map(process_range, tasks)
        processing_wall_ms = (time.perf_counter() - processing_started) * 1000
    finally:
        pool.close()
        pool.join()

    return {
        "record_count": sum(result["record_count"] for result in results),
        "value_sum": sum(result["value_sum"] for result in results),
        "bytes_processed": sum(result["bytes_processed"] for result in results),
        "range_bytes": sum(result["range_bytes"] for result in results),
        "worker_startup_ms": startup_ms,
        "processing_wall_ms": processing_wall_ms,
        "worker_processing_ms": max(result["worker_ms"] for result in results),
        "worker_count": len(results),
    }


def median(values: list[float]) -> float:
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2


def benchmark(path: Path, lib: ctypes.CDLL, workers: list[int], repeats: int) -> None:
    reference_runs = [single_process_reference(path) for _ in range(repeats)]
    reference = reference_runs[0]
    for run in reference_runs[1:]:
        if run["record_count"] != reference["record_count"] or run["value_sum"] != reference["value_sum"]:
            raise AssertionError("single-process reference was not deterministic")

    print(json.dumps({"type": "reference", "median": {
        "record_count": reference["record_count"],
        "value_sum": reference["value_sum"],
        "bytes_processed": reference["bytes_processed"],
        "wall_ms": median([run["wall_ms"] for run in reference_runs]),
    }}, sort_keys=True))

    for worker_count in workers:
        plans = [plan_ranges(lib, path, worker_count) for _ in range(repeats)]
        plan = plans[0]
        planning_ms = median([item["planning_total_ms"] for item in plans])
        planning_core_ms = median([item["partition_planning_ms"] for item in plans])
        run_results = [multiprocessing_run(path, plan["ranges"], worker_count) for _ in range(repeats)]

        for result in run_results:
            observed = (result["record_count"], result["value_sum"], result["range_bytes"])
            expected = (reference["record_count"], reference["value_sum"], reference["bytes_processed"])
            if observed != expected or result["bytes_processed"] != reference["bytes_processed"]:
                raise AssertionError(f"worker result mismatch: {observed} != {expected}")

        print(json.dumps({"type": "multiprocessing", "workers": worker_count, "partitions": plan["actual_partitions"], "partition_lengths": plan["lengths"], "median": {
            "planning_total_ms": planning_ms,
            "partition_planning_ms": planning_core_ms,
            "worker_startup_ms": median([run["worker_startup_ms"] for run in run_results]),
            "processing_wall_ms": median([run["processing_wall_ms"] for run in run_results]),
            "worker_processing_ms": median([run["worker_processing_ms"] for run in run_results]),
            "end_to_end_ms": median([planning_ms + run["worker_startup_ms"] + run["processing_wall_ms"] for run in run_results]),
            "record_count": reference["record_count"],
            "bytes_processed": reference["bytes_processed"],
            "value_sum": reference["value_sum"],
        }}, sort_keys=True))


def parse_workers(raw: str, cpu_count: int | None) -> list[int]:
    available = max(cpu_count or 1, 1)
    values = sorted({max(1, int(value)) for value in raw.split(",")})
    return [value for value in values if value <= available and value <= 8] or [1]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", type=Path, help="path to the built mmap-chunker-core dynamic library")
    parser.add_argument("--input", type=Path, help="existing JSONL file; otherwise generate a deterministic fixture")
    parser.add_argument("--records", type=int, default=100_000)
    parser.add_argument("--payload-bytes", type=int, default=64)
    parser.add_argument("--workers", default="1,2,4")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()

    if args.repeats < 1 or args.records < 1 or args.payload_bytes < 0:
        parser.error("records, payload-bytes, and repeats must be non-negative with records/repeats > 0")

    root = Path(__file__).resolve().parents[1]
    library = args.library or root / "target" / "release" / library_name()
    workers = parse_workers(args.workers, os.cpu_count())
    print(json.dumps({"type": "metadata", "platform": platform.platform(), "python": sys.version.split()[0], "cpu_count": os.cpu_count(), "workers": workers, "repeats": args.repeats, "library": str(library), "abi_expected": "0x00010003", "capability_record_partitioning": 1 << 4}, sort_keys=True))

    lib = load_library(library)
    abi = int(lib.mmap_engine_abi_version())
    capabilities = int(lib.mmap_engine_capabilities())
    if abi != 0x00010003 or not capabilities & (1 << 4):
        raise RuntimeError(f"unsupported library: abi=0x{abi:08x}, capabilities=0x{capabilities:08x}")

    if args.input:
        benchmark(args.input, lib, workers, args.repeats)
        return

    with tempfile.TemporaryDirectory(prefix="mmap_chunker_jsonl_proof_") as temp_dir:
        path = Path(temp_dir) / "records.jsonl"
        generate_jsonl(path, args.records, args.payload_bytes)
        print(json.dumps({"type": "workload", "path": str(path), "records_requested": args.records, "payload_bytes": args.payload_bytes, "file_size": path.stat().st_size}, sort_keys=True))
        benchmark(path, lib, workers, args.repeats)


if __name__ == "__main__":
    main()
