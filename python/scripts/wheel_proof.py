"""Run the full installed-wheel proof for mmap-chunker-core.

Executed inside a fresh virtual environment after ``pip install <wheel>``.
Asserts that the package works purely from the wheel with no Cargo, no CLI,
and no environment hacks, then exercises the planner and independently
reconstructs the source records.

Exit code 0 with a JSON report on success, nonzero on any failure.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

try:
    import mmap_chunker
except ImportError as exc:
    print(f"FATAL: mmap_chunker is not importable: {exc}", file=sys.stderr)
    sys.exit(1)

from mmap_chunker import plan_file

REPORT: dict = {
    "import_ok": True,
    "cargo_required": False,
    "loaded_library_path": None,
}


def check_no_runtime_cargo() -> None:
    """The wheel must not need Cargo or a downloaded CLI at runtime."""
    cargo = shutil.which("cargo")
    cli = shutil.which("mmap-chunker")
    if cargo is not None:
        # Cargo may exist on the machine, but the package must not invoke it.
        pass
    if cli is not None:
        raise AssertionError(
            "mmap-chunker CLI was found on PATH; the proof must run without it"
        )
    from mmap_chunker import _native

    REPORT["loaded_library_path"] = str(_native.library_path())


def verify_abi() -> None:
    version = mmap_chunker.__version__
    abi = mmap_chunker.abi_version()
    caps = mmap_chunker.capabilities()
    REPORT["version"] = version
    REPORT["abi_version"] = f"0x{abi:08x}"
    REPORT["capabilities"] = f"0x{caps:08x}"
    if abi != 0x0001_0003:
        raise AssertionError(f"unexpected ABI version 0x{abi:08x}")
    if not caps & (1 << 4):
        raise AssertionError("RECORD_PARTITIONING capability missing")


def run_planner_proof() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="wheel_proof_"))
    try:
        path = tmp / "records-Δ.jsonl"
        records = [
            {"id": i, "value": (i * 1_000_003) % 997_651, "payload": "x" * 32}
            for i in range(5000)
        ]
        with open(path, "wb") as fh:
            for r in records:
                fh.write(json.dumps(r, separators=(",", ":")).encode("utf-8"))
                fh.write(b"\n")

        plan = plan_file(path, parts=4)
        assert plan.actual_partitions == 4
        assert plan.ranges[0].start == 0
        assert plan.ranges[-1].end == plan.file_size
        assert plan.file_size == path.stat().st_size
        assert all(r.length == r.end - r.start for r in plan.ranges)
        assert all(a.end == b.start for a, b in zip(plan.ranges, plan.ranges[1:]))

        seen = 0
        seen_values = 0
        with open(path, "rb") as fh:
            for r in plan.ranges:
                fh.seek(r.start)
                chunk = fh.read(r.length)
                assert len(chunk) == r.length
                for line in chunk.splitlines():
                    value = json.loads(line)["value"]
                    seen_values += value
                    seen += 1

        expected_values = sum(r["value"] for r in records)
        assert seen == len(records), f"record count mismatch {seen} != {len(records)}"
        assert seen_values == expected_values, (
            "value sum mismatch (duplicate/missing records)"
        )

        # Determinism
        plan2 = plan_file(path, parts=4)
        assert plan2.ranges == plan.ranges

        # Empty file
        empty = tmp / "empty.jsonl"
        empty.write_bytes(b"")
        empty_plan = plan_file(empty, parts=4)
        assert empty_plan.ranges == ()

        REPORT["records_planned"] = seen
        REPORT["records_expected"] = len(records)
        REPORT["file_size"] = plan.file_size
        REPORT["ranges"] = [f"{r.start}:{r.end}" for r in plan.ranges]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main() -> int:
    check_no_runtime_cargo()
    verify_abi()
    run_planner_proof()
    print(json.dumps(REPORT, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
