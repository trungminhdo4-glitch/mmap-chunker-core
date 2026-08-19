"""Measure planning overhead: Python API vs the standalone CLI subprocess.

Reports native planning time, Python call overhead, total plan_file wall time,
and CLI subprocess wall time on a bounded fixture. Deterministic and bounded;
does not optimize the Rust scanner.
"""

from __future__ import annotations

import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from mmap_chunker import plan_file

_REPO = Path(__file__).resolve().parents[2]
CLI = (
    _REPO
    / "target"
    / "release"
    / ("mmap-chunker.exe" if os.name == "nt" else "mmap-chunker")
)


def _fixture(root: Path, records: int, payload: int) -> Path:
    path = root / "overhead.jsonl"
    payload_bytes = b"x" * payload
    with open(path, "wb") as fh:
        for i in range(records):
            fh.write(b'{"id":' + str(i).encode() + b',"p":' + payload_bytes + b"}\n")
    return path


def median(values: list[float]) -> float:
    ordered = sorted(values)
    mid = len(ordered) // 2
    return ordered[mid] if len(ordered) % 2 else (ordered[mid - 1] + ordered[mid]) / 2


def main() -> int:
    root = Path(tempfile.mkdtemp(prefix="planning_overhead_"))
    path = _fixture(root, 200_000, 64)
    repeats = 20

    # Native-only planning (partition + get_chunk loop) through ctypes.
    from mmap_chunker import _native

    lib = _native.get_library()
    native_times: list[float] = []
    api_times: list[float] = []
    for _ in range(repeats):
        handle = lib.mmap_engine_open(os.fsencode(path))
        if not handle:
            raise RuntimeError("open failed")
        t0 = time.perf_counter()
        count = int(lib.mmap_engine_partition_records(handle, 8, 0x0A))
        view = _native._CChunkView()
        for i in range(count):
            lib.mmap_engine_get_chunk(handle, i, ctypes.byref(view))
        native_times.append((time.perf_counter() - t0) * 1000)
        lib.mmap_engine_free(handle)

        t0 = time.perf_counter()
        plan_file(path, parts=8)
        api_times.append((time.perf_counter() - t0) * 1000)

    # CLI subprocess wall time.
    cli_times: list[float] = []
    for _ in range(repeats):
        t0 = time.perf_counter()
        completed = subprocess.run(
            [str(CLI), "partition", str(path), "--parts", "8"],
            capture_output=True,
            check=True,
            timeout=120,
        )
        cli_times.append((time.perf_counter() - t0) * 1000)

    report = {
        "fixture_records": 200_000,
        "fixture_bytes": path.stat().st_size,
        "native_planning_ms": round(median(native_times), 3),
        "plan_file_total_ms": round(median(api_times), 3),
        "cli_subprocess_ms": round(median(cli_times), 3),
        "python_overhead_ms": round(median(api_times) - median(native_times), 3),
        "cli_vs_api_x": round(median(cli_times) / median(api_times), 2),
    }
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
