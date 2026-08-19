#!/usr/bin/env python3
"""Small real consumer for the ``partition-files`` CLI contract.

The CLI plans record-aligned ranges. This dependency-free example keeps the
ordered source list, groups the five-column plan by worker, and starts
independent ``spawn`` workers. Each worker opens only its assigned source paths
and parses only its assigned source-local byte ranges.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
import json
import multiprocessing as mp
import os
from pathlib import Path
import subprocess
import sys
import time
from typing import Any, Callable


DEFAULT_DELIMITER = 0x0A
PLANNER_TIMEOUT = 120.0
WORKER_TIMEOUT = 120.0


@dataclass(frozen=True)
class RangeRow:
    worker_index: int
    source_index: int
    start: int
    end_exclusive: int
    length: int


def parse_plan(stdout: bytes) -> list[RangeRow]:
    """Parse the headerless five-column TSV emitted by the planner."""

    try:
        text = stdout.decode("ascii")
    except UnicodeDecodeError as error:
        raise AssertionError("planner TSV was not ASCII") from error

    rows: list[RangeRow] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        fields = line.split("\t")
        if len(fields) != 5:
            raise AssertionError(
                f"line {line_number} has {len(fields)} fields, expected five"
            )
        try:
            values = [int(field) for field in fields]
        except ValueError as error:
            raise AssertionError(
                f"line {line_number} contains a non-numeric field"
            ) from error
        rows.append(RangeRow(*values))
    return rows


def group_rows_by_worker(rows: list[RangeRow]) -> dict[int, list[RangeRow]]:
    grouped: dict[int, list[RangeRow]] = {}
    for row in rows:
        grouped.setdefault(row.worker_index, []).append(row)
    return grouped


def decode_records(data: bytes, delimiter: int) -> list[dict[str, Any]]:
    parts = data.split(bytes((delimiter,)))
    if parts and parts[-1] == b"":
        parts.pop()
    if any(part == b"" for part in parts):
        raise ValueError("assigned range contains an empty JSONL record")

    records: list[dict[str, Any]] = []
    for raw in parts:
        value = json.loads(raw)
        if not isinstance(value, dict):
            raise ValueError("assigned record is not a JSON object")
        if not isinstance(value.get("id"), int):
            raise ValueError("assigned record has no integer id")
        if not isinstance(value.get("value"), int):
            raise ValueError("assigned record has no integer value")
        records.append(value)
    return records


WorkerTask = tuple[int, tuple[str, ...], list[RangeRow], int]
WorkerTarget = Callable[[WorkerTask], dict[str, Any]]


def worker_main(task: WorkerTask) -> dict[str, Any]:
    worker_index, source_paths, rows, delimiter = task
    handles: dict[int, Any] = {}
    try:
        for source_index in sorted({row.source_index for row in rows}):
            try:
                handles[source_index] = open(source_paths[source_index], "rb")
            except OSError as error:
                raise OSError(
                    f"worker={worker_index} source={source_index} open failed: {error}"
                ) from error

        started = time.perf_counter()
        observed_keys: Counter[tuple[int, int]] = Counter()
        value_sum = 0
        processed_bytes = 0
        for row in rows:
            handle = handles[row.source_index]
            handle.seek(row.start)
            data = handle.read(row.length)
            if len(data) != row.length:
                raise IOError(
                    f"worker={worker_index} source={row.source_index} short read: "
                    f"expected {row.length}, got {len(data)}"
                )
            processed_bytes += len(data)
            try:
                records = decode_records(data, delimiter)
            except (TypeError, ValueError) as error:
                raise ValueError(
                    f"worker={worker_index} source={row.source_index} parse failed: {error}"
                ) from error
            for record in records:
                observed_keys[(row.source_index, int(record["id"]))] += 1
                value_sum += int(record["value"])

        return {
            "ok": True,
            "worker": worker_index,
            "record_count": sum(observed_keys.values()),
            "value_sum": value_sum,
            "processed_bytes": processed_bytes,
            "keys": list(observed_keys.elements()),
            "worker_ms": (time.perf_counter() - started) * 1000.0,
        }
    except BaseException as error:  # returned so the parent can add worker context
        return {
            "ok": False,
            "worker": worker_index,
            "error": f"{type(error).__name__}: {error}",
        }
    finally:
        for handle in handles.values():
            handle.close()


def execute_workers(
    paths: tuple[Path, ...],
    rows: list[RangeRow],
    delimiter: int,
    *,
    worker_target: WorkerTarget = worker_main,
    worker_timeout: float = WORKER_TIMEOUT,
) -> dict[str, Any]:
    grouped = group_rows_by_worker(rows)
    if not grouped:
        return {
            "record_count": 0,
            "value_sum": 0,
            "processed_bytes": 0,
            "worker_startup_ms": 0.0,
            "processing_ms": 0.0,
            "worker_processing_ms": 0.0,
            "observed_keys": Counter(),
        }

    source_paths = tuple(str(path) for path in paths)
    tasks = [
        (worker_index, source_paths, grouped[worker_index], delimiter)
        for worker_index in sorted(grouped)
    ]
    context = mp.get_context("spawn")
    startup_started = time.perf_counter()
    pool = context.Pool(processes=len(tasks))
    startup_ms = (time.perf_counter() - startup_started) * 1000.0
    clean_exit = False
    try:
        processing_started = time.perf_counter()
        pending = pool.map_async(worker_target, tasks)
        try:
            # Read all results before close/join; joining first can deadlock
            # when a child is still flushing a multiprocessing queue.
            results = pending.get(worker_timeout)
        except mp.TimeoutError as error:
            raise TimeoutError(
                f"worker phase timed out after {worker_timeout:.1f}s; "
                f"workers={sorted(grouped)}"
            ) from error
        processing_ms = (time.perf_counter() - processing_started) * 1000.0
        pool.close()
        clean_exit = True
    finally:
        if not clean_exit:
            pool.terminate()
        pool.join()

    failures = sorted(
        (result for result in results if not result.get("ok")),
        key=lambda result: int(result["worker"]),
    )
    if failures:
        failure = failures[0]
        raise RuntimeError(
            f"worker phase failed: worker={failure['worker']} error={failure['error']}"
        )

    observed_keys: Counter[tuple[int, int]] = Counter()
    for result in results:
        observed_keys.update(tuple(key) for key in result["keys"])
    return {
        "record_count": sum(int(result["record_count"]) for result in results),
        "value_sum": sum(int(result["value_sum"]) for result in results),
        "processed_bytes": sum(int(result["processed_bytes"]) for result in results),
        "worker_startup_ms": startup_ms,
        "processing_ms": processing_ms,
        "worker_processing_ms": max(float(result["worker_ms"]) for result in results),
        "observed_keys": observed_keys,
    }


def _byte_value(raw: str) -> int:
    value = int(raw)
    if not 0 <= value <= 255:
        raise argparse.ArgumentTypeError("delimiter byte must be in 0..255")
    return value


def invoke_planner(cli: Path, paths: tuple[Path, ...], parts: int, delimiter: int) -> bytes:
    arguments = [str(cli), "partition-files", "--parts", str(parts)]
    if delimiter != DEFAULT_DELIMITER:
        arguments.extend(["--delimiter-byte", str(delimiter)])
    arguments.extend(str(path) for path in paths)
    try:
        completed = subprocess.run(
            arguments,
            capture_output=True,
            check=False,
            timeout=PLANNER_TIMEOUT,
        )
    except subprocess.TimeoutExpired as error:
        raise TimeoutError(
            f"planner phase timed out after {PLANNER_TIMEOUT:.1f}s: {cli}"
        ) from error
    except OSError as error:
        raise RuntimeError(f"planner phase could not execute {cli}: {error}") from error
    if completed.returncode != 0:
        stderr = completed.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(f"planner phase failed: status={completed.returncode} stderr={stderr}")
    if completed.stderr:
        raise RuntimeError(f"planner phase wrote unexpected stderr: {completed.stderr!r}")
    return completed.stdout


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cli",
        type=Path,
        help="standalone mmap-chunker executable (default: target/release/mmap-chunker[.exe])",
    )
    parser.add_argument("--parts", type=int, required=True, help="requested worker count")
    parser.add_argument("--delimiter-byte", type=_byte_value, default=DEFAULT_DELIMITER)
    parser.add_argument("paths", nargs="+", type=Path, help="ordered JSONL source paths")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.parts < 1:
        parser.error("--parts must be positive")

    root = Path(__file__).resolve().parents[1]
    cli = args.cli or root / "target" / "release" / (
        "mmap-chunker.exe" if os.name == "nt" else "mmap-chunker"
    )
    paths = tuple(args.paths)
    try:
        rows = parse_plan(invoke_planner(cli, paths, args.parts, args.delimiter_byte))
        result = execute_workers(paths, rows, args.delimiter_byte)
    except (AssertionError, OSError, RuntimeError, TimeoutError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(
        json.dumps(
            {
                "source_count": len(paths),
                "requested_workers": args.parts,
                "actual_workers": len(group_rows_by_worker(rows)),
                "ranges": len(rows),
                "record_count": result["record_count"],
                "value_sum": result["value_sum"],
                "processed_bytes": result["processed_bytes"],
                "worker_startup_ms": result["worker_startup_ms"],
                "processing_ms": result["processing_ms"],
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    mp.freeze_support()
    raise SystemExit(main())
