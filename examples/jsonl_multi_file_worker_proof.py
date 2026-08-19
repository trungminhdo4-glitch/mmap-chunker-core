#!/usr/bin/env python3
"""Bounded real-worker proof for the ``partition-files`` CLI contract.

The CLI is the planner.  This example is an independent consumer: it creates
ordered local JSONL shards, invokes the standalone CLI, groups its five-column
TSV by worker, and starts independent ``spawn`` workers.  Workers open only
their assigned source paths and read only their assigned source-local byte
ranges.  The parent validates the result against a separate source-by-source
oracle.

The proof deliberately does not add a worker pool, parser, or multi-file API to
the Rust core.  JSON is used only by this example's consumer/oracle.
"""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from typing import Any

import jsonl_multi_file_workers as reference
from jsonl_multi_file_workers import RangeRow, decode_records, execute_workers
from jsonl_multi_file_workers import group_rows_by_worker, parse_plan


MAX_SUPPORTED_WORKERS = 16
DEFAULT_DELIMITER = 0x0A
PLANNER_TIMEOUT = 120.0


@dataclass(frozen=True)
class Scenario:
    name: str
    paths: tuple[Path, ...]
    delimiter: int


def median(values: list[float]) -> float:
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2


def parse_int_list(raw: str, label: str, maximum: int | None = None) -> list[int]:
    values: list[int] = []
    for item in raw.split(","):
        try:
            value = int(item)
        except ValueError as error:
            raise ValueError(f"{label} contains a non-integer: {item!r}") from error
        if value < 1:
            raise ValueError(f"{label} values must be positive")
        if maximum is not None and value > maximum:
            raise ValueError(f"{label} values must be <= {maximum}")
        if value not in values:
            values.append(value)
    if not values:
        raise ValueError(f"{label} must not be empty")
    return values


def json_record(record_id: int, value: int, payload_bytes: int) -> bytes:
    return json.dumps(
        {
            "id": record_id,
            "value": value,
            "payload": "x" * payload_bytes,
        },
        separators=(",", ":"),
    ).encode("utf-8")


def write_records(
    path: Path,
    record_ids: list[int],
    delimiter: int = DEFAULT_DELIMITER,
    payload_bytes: int = 8,
    final_delimiter: bool = True,
    payload_by_index: list[int] | None = None,
) -> None:
    delimiter_bytes = bytes((delimiter,))
    with path.open("wb") as output:
        for index, record_id in enumerate(record_ids):
            payload_size = payload_bytes
            if payload_by_index is not None:
                payload_size = payload_by_index[index]
            output.write(json_record(record_id, (record_id * 1_000_003) % 997_651, payload_size))
            if final_delimiter or index < len(record_ids) - 1:
                output.write(delimiter_bytes)


def expected_oracle(paths: tuple[Path, ...], delimiter: int) -> dict[str, Any]:
    source_data = [path.read_bytes() for path in paths]
    expected_keys: Counter[tuple[int, int]] = Counter()
    record_count = 0
    value_sum = 0
    for source_index, data in enumerate(source_data):
        for record in decode_records(data, delimiter):
            key = (source_index, int(record["id"]))
            if key in expected_keys:
                raise AssertionError(f"duplicate fixture record key: {key}")
            expected_keys[key] += 1
            record_count += 1
            value_sum += int(record["value"])

    return {
        "source_data": source_data,
        "source_sizes": [len(data) for data in source_data],
        "expected_keys": expected_keys,
        "record_count": record_count,
        "value_sum": value_sum,
        "total_bytes": sum(len(data) for data in source_data),
        "checksum": checksum(expected_keys),
    }


def checksum(keys: Counter[tuple[int, int]]) -> str:
    digest = hashlib.sha256()
    for (source_index, record_id), count in sorted(keys.items()):
        digest.update(f"{source_index}:{record_id}:{count}\n".encode("ascii"))
    return digest.hexdigest()


def invoke_planner(cli: Path, paths: tuple[Path, ...], parts: int, delimiter: int) -> tuple[bytes, float]:
    arguments = [str(cli), "partition-files", "--parts", str(parts)]
    if delimiter != DEFAULT_DELIMITER:
        arguments.extend(["--delimiter-byte", str(delimiter)])
    arguments.extend(str(path) for path in paths)
    started = time.perf_counter()
    try:
        completed = subprocess.run(
            arguments,
            capture_output=True,
            check=False,
            timeout=PLANNER_TIMEOUT,
        )
    except subprocess.TimeoutExpired as error:
        raise TimeoutError(
            f"planner timed out after {PLANNER_TIMEOUT:.1f}s: {cli}"
        ) from error
    except OSError as error:
        raise RuntimeError(f"could not execute planner {cli}: {error}") from error
    planning_ms = (time.perf_counter() - started) * 1000.0
    if completed.returncode != 0:
        raise RuntimeError(
            "planner failed: "
            f"status={completed.returncode} stderr={completed.stderr.decode('utf-8', 'replace')}"
        )
    if completed.stderr:
        raise AssertionError(f"planner wrote unexpected stderr: {completed.stderr!r}")
    return completed.stdout, planning_ms


def validate_plan(
    rows: list[RangeRow],
    oracle: dict[str, Any],
    delimiter: int,
) -> dict[str, Any]:
    source_data: list[bytes] = oracle["source_data"]
    source_cursors = [0] * len(source_data)
    worker_bytes: Counter[int] = Counter()
    previous_sort_key: tuple[int, int, int] | None = None

    for row in rows:
        if row.worker_index < 0 or row.source_index < 0:
            raise AssertionError(f"negative planner index: {row}")
        if row.source_index >= len(source_data):
            raise AssertionError(f"source index out of range: {row}")
        source = source_data[row.source_index]
        if row.start < 0 or row.start > row.end_exclusive or row.end_exclusive > len(source):
            raise AssertionError(f"invalid source-local range: {row}")
        if row.end_exclusive - row.start != row.length:
            raise AssertionError(f"length arithmetic mismatch: {row}")
        if row.length <= 0:
            raise AssertionError(f"planner emitted an empty range: {row}")
        if row.start not in (0, len(source)) and source[row.start - 1] != delimiter:
            raise AssertionError(f"range start splits a record: {row}")
        if row.end_exclusive not in (0, len(source)) and source[row.end_exclusive - 1] != delimiter:
            raise AssertionError(f"range end splits a record: {row}")

        sort_key = (row.worker_index, row.source_index, row.start)
        if previous_sort_key is not None and sort_key < previous_sort_key:
            raise AssertionError("planner rows are not ordered by worker/source/start")
        previous_sort_key = sort_key
        if row.start != source_cursors[row.source_index]:
            raise AssertionError(
                f"gap or overlap for source {row.source_index}: "
                f"expected {source_cursors[row.source_index]}, got {row.start}"
            )
        source_cursors[row.source_index] = row.end_exclusive
        worker_bytes[row.worker_index] += row.length

    for source_index, (cursor, source) in enumerate(zip(source_cursors, source_data)):
        if cursor != len(source):
            raise AssertionError(f"source {source_index} coverage ends at {cursor}, expected {len(source)}")

    workers = group_rows_by_worker(rows)
    worker_indices = sorted(workers)
    if worker_indices != list(range(len(worker_indices))):
        raise AssertionError(f"worker indices are not compact: {worker_indices}")
    expected_total = oracle["total_bytes"]
    if sum(worker_bytes.values()) != expected_total:
        raise AssertionError("worker byte totals do not cover the logical dataset")

    return {
        "ranges_total": len(rows),
        "ranges_per_worker": [len(workers[index]) for index in worker_indices],
        "bytes_per_worker": [worker_bytes[index] for index in worker_indices],
        "actual_workers": len(worker_indices),
        "coverage_ok": True,
        "boundary_ok": True,
    }


def run_case(cli: Path, scenario: Scenario, requested_workers: int, repeats: int) -> dict[str, Any]:
    oracle = expected_oracle(scenario.paths, scenario.delimiter)
    first_plan: bytes | None = None
    planning_times: list[float] = []
    rows: list[RangeRow] | None = None
    plan_checks: dict[str, Any] | None = None

    for _ in range(repeats):
        output, planning_ms = invoke_planner(
            cli, scenario.paths, requested_workers, scenario.delimiter
        )
        planning_times.append(planning_ms)
        if rows is None:
            first_plan = output
            rows = parse_plan(output)
            plan_checks = validate_plan(rows, oracle, scenario.delimiter)
        elif output != first_plan:
            raise AssertionError("repeated planner output was not byte-identical")

    assert rows is not None
    assert plan_checks is not None
    worker_runs = [execute_workers(scenario.paths, rows, scenario.delimiter) for _ in range(repeats)]
    expected_keys: Counter[tuple[int, int]] = oracle["expected_keys"]
    for result in worker_runs:
        exact_once = result["observed_keys"] == expected_keys
        checksum_ok = checksum(result["observed_keys"]) == oracle["checksum"]
        if not exact_once or not checksum_ok:
            raise AssertionError(
                f"worker oracle mismatch: exact_once={exact_once} checksum_ok={checksum_ok}"
            )
        if result["record_count"] != oracle["record_count"]:
            raise AssertionError("worker record count differs from independent oracle")
        if result["value_sum"] != oracle["value_sum"]:
            raise AssertionError("worker numeric aggregate differs from independent oracle")
        if result["processed_bytes"] != oracle["total_bytes"]:
            raise AssertionError("worker processed-byte total differs from source coverage")

    worker_bytes = plan_checks["bytes_per_worker"]
    ideal_worker_bytes = (
        oracle["total_bytes"] / plan_checks["actual_workers"] if plan_checks["actual_workers"] else 0.0
    )
    max_worker_bytes = max(worker_bytes, default=0)
    min_worker_bytes = min(worker_bytes, default=0)
    return {
        "scenario": scenario.name,
        "source_files": len(scenario.paths),
        "record_count": oracle["record_count"],
        "total_bytes": oracle["total_bytes"],
        "requested_workers": requested_workers,
        "actual_workers": plan_checks["actual_workers"],
        "ranges_total": plan_checks["ranges_total"],
        "ranges_per_worker": plan_checks["ranges_per_worker"],
        "bytes_per_worker": worker_bytes,
        "min_worker_bytes": min_worker_bytes,
        "max_worker_bytes": max_worker_bytes,
        "ideal_worker_bytes": ideal_worker_bytes,
        "imbalance_ratio": (max_worker_bytes / ideal_worker_bytes) if ideal_worker_bytes else 0.0,
        "planning_ms": median(planning_times),
        "worker_startup_ms": median([run["worker_startup_ms"] for run in worker_runs]),
        "processing_ms": median([run["processing_ms"] for run in worker_runs]),
        "worker_processing_ms": median([run["worker_processing_ms"] for run in worker_runs]),
        "end_to_end_ms": median(
            [
                planning + run["worker_startup_ms"] + run["processing_ms"]
                for planning, run in zip(planning_times, worker_runs)
            ]
        ),
        "determinism": True,
        "coverage_ok": plan_checks["coverage_ok"],
        "boundary_ok": plan_checks["boundary_ok"],
        "exact_once": True,
        "checksum_ok": True,
        "checksum": oracle["checksum"],
        "numeric_aggregate": oracle["value_sum"],
        "total_processed_bytes": oracle["total_bytes"],
        "delimiter": scenario.delimiter,
    }


def crashing_worker(task: tuple[Any, ...]) -> dict[str, Any]:
    os._exit(17)


def hanging_worker(task: tuple[Any, ...]) -> dict[str, Any]:
    while True:
        time.sleep(1)


def run_failure_probe(cli: Path, paths: tuple[Path, ...], delimiter: int) -> dict[str, Any]:
    missing = paths[1].parent / "definitely-missing-worker-proof-source.jsonl"
    cases = [
        (
            "missing_source",
            [str(cli), "partition-files", "--parts", "2", str(paths[1]), str(missing)],
        ),
        (
            "invalid_parts",
            [str(cli), "partition-files", "--parts", "0", str(paths[1])],
        ),
    ]
    results: dict[str, Any] = {}
    for name, arguments in cases:
        completed = subprocess.run(arguments, capture_output=True, check=False)
        if completed.returncode == 0 or completed.stdout:
            raise AssertionError(f"{name} probe unexpectedly succeeded or emitted stdout")
        results[name] = {
            "failed": True,
            "stdout_empty": not completed.stdout,
            "stderr_nonempty": bool(completed.stderr),
        }

    try:
        invoke_planner(cli.parent / "definitely-missing-mmap-chunker", paths, 2, delimiter)
    except RuntimeError as error:
        results["missing_planner"] = {"failed": True, "phase_context": "planner" in str(error)}
    else:
        raise AssertionError("missing_planner probe unexpectedly succeeded")

    try:
        parse_plan(b"0\t1\t2")
    except AssertionError:
        results["malformed_tsv"] = {"failed": True}
    else:
        raise AssertionError("malformed_tsv probe unexpectedly succeeded")

    output, _ = invoke_planner(cli, paths, 2, delimiter)
    rows = parse_plan(output)
    try:
        reference.execute_workers(
            paths,
            rows,
            delimiter,
            worker_target=crashing_worker,
            worker_timeout=2.0,
        )
    except (RuntimeError, TimeoutError) as error:
        results["worker_process_failure"] = {
            "failed": True,
            "phase_context": "phase" in str(error),
        }
    else:
        raise AssertionError("worker_process_failure probe unexpectedly succeeded")

    try:
        reference.execute_workers(
            paths,
            rows,
            delimiter,
            worker_target=hanging_worker,
            worker_timeout=0.2,
        )
    except TimeoutError as error:
        results["worker_timeout"] = {
            "failed": True,
            "phase_context": "phase" in str(error),
        }
    else:
        raise AssertionError("worker_timeout probe unexpectedly succeeded")

    original = paths[1].read_bytes()
    paths[1].write_bytes(original[:-1])
    try:
        try:
            reference.execute_workers(paths, rows, delimiter, worker_timeout=2.0)
        except RuntimeError as error:
            results["short_read_after_mutation"] = {
                "failed": True,
                "source_context": "source=" in str(error),
            }
        else:
            raise AssertionError("short_read_after_mutation probe unexpectedly succeeded")
    finally:
        paths[1].write_bytes(original)

    return results


def make_uniform_sources(directory: Path, count: int) -> tuple[Path, ...]:
    paths: list[Path] = []
    record_count = 64 if count == 1 else (8 if count < 256 else 4)
    for source_index in range(count):
        path = directory / f"uniform-{source_index:03d}.jsonl"
        record_ids = [source_index * 1_000_000 + index for index in range(record_count)]
        write_records(path, record_ids, payload_bytes=8 + (source_index % 3))
        paths.append(path)
    return tuple(paths)


def make_edge_sources(directory: Path) -> tuple[Path, ...]:
    paths: list[Path] = []
    empty = directory / "z-empty.jsonl"
    empty.write_bytes(b"")
    paths.append(empty)

    uneven = directory / "b-uneven.jsonl"
    write_records(uneven, [10, 11, 12], payload_by_index=[1, 32, 2])
    paths.append(uneven)

    giant = directory / "m-giant.jsonl"
    write_records(giant, [20], payload_bytes=4096)
    paths.append(giant)

    no_final = directory / "a-no-final-newline.jsonl"
    write_records(no_final, [30, 31, 32], final_delimiter=False, payload_bytes=5)
    paths.append(no_final)

    small = directory / "q-small.jsonl"
    write_records(small, [40, 41, 42, 43], payload_bytes=3)
    paths.append(small)
    return tuple(paths)


def make_custom_delimiter_sources(directory: Path) -> tuple[Path, ...]:
    paths: list[Path] = []
    delimiter = 0x1E
    for source_index, name in enumerate(("z-custom.bin", "a-custom.bin")):
        path = directory / name
        write_records(
            path,
            [100 + source_index * 10 + index for index in range(5)],
            delimiter=delimiter,
            payload_bytes=4 + source_index,
            final_delimiter=source_index == 0,
        )
        paths.append(path)
    return tuple(paths)


def make_duplicate_sources(directory: Path) -> tuple[Path, ...]:
    first = directory / "z-first.jsonl"
    second = directory / "a-second.jsonl"
    write_records(first, [200, 201, 202], payload_bytes=6)
    write_records(second, [300, 301], payload_bytes=9)
    # The non-lexical order and repeated path are intentional logical sources.
    return (first, second, first)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cli",
        type=Path,
        help="standalone mmap-chunker executable (default: target/release/mmap-chunker[.exe])",
    )
    parser.add_argument("--source-counts", default="1,16,64")
    parser.add_argument("--include-256", action="store_true")
    parser.add_argument("--workers", default="1,2,4,8,16")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--skip-failure-probes", action="store_true")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    try:
        source_counts = parse_int_list(args.source_counts, "--source-counts")
        if args.include_256 and 256 not in source_counts:
            source_counts.append(256)
        workers = parse_int_list(
            args.workers, "--workers", maximum=MAX_SUPPORTED_WORKERS
        )
    except ValueError as error:
        parser.error(str(error))

    root = Path(__file__).resolve().parents[1]
    cli = args.cli or root / "target" / "release" / (
        "mmap-chunker.exe" if os.name == "nt" else "mmap-chunker"
    )
    if not cli.is_file():
        parser.error(f"planner executable not found: {cli} (run cargo build --release first)")

    print(
        json.dumps(
            {
                "type": "metadata",
                "platform": sys.platform,
                "python": sys.version.split()[0],
                "start_method": "spawn",
                "cli": str(cli),
                "source_counts": source_counts,
                "workers": workers,
                "repeats": args.repeats,
            },
            sort_keys=True,
        )
    )

    result_rows: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="mmap_chunker_worker_proof_") as temp_dir:
        directory = Path(temp_dir)
        for source_count in source_counts:
            case_directory = directory / f"uniform-{source_count}"
            case_directory.mkdir()
            paths = make_uniform_sources(case_directory, source_count)
            for requested_workers in workers:
                result = run_case(
                    cli,
                    Scenario(f"uniform_{source_count}", paths, DEFAULT_DELIMITER),
                    requested_workers,
                    args.repeats,
                )
                result_rows.append(result)
                print(json.dumps(result, sort_keys=True), flush=True)

        edge_directory = directory / "edge"
        edge_directory.mkdir()
        edge_paths = make_edge_sources(edge_directory)
        edge_workers = sorted(
            {workers[0], workers[min(1, len(workers) - 1)], workers[-1], MAX_SUPPORTED_WORKERS}
        )
        for requested_workers in edge_workers:
            result = run_case(
                cli,
                Scenario("edge_records", edge_paths, DEFAULT_DELIMITER),
                requested_workers,
                args.repeats,
            )
            result_rows.append(result)
            print(json.dumps(result, sort_keys=True), flush=True)
        if not any(row["actual_workers"] < row["requested_workers"] for row in result_rows if row["scenario"] == "edge_records"):
            raise AssertionError("edge fixture did not demonstrate collapsed worker targets")

        custom_directory = directory / "custom"
        custom_directory.mkdir()
        custom_paths = make_custom_delimiter_sources(custom_directory)
        custom_result = run_case(
            cli,
            Scenario("custom_delimiter", custom_paths, 0x1E),
            min(4, max(workers)),
            args.repeats,
        )
        result_rows.append(custom_result)
        print(json.dumps(custom_result, sort_keys=True), flush=True)

        duplicate_directory = directory / "duplicate"
        duplicate_directory.mkdir()
        duplicate_paths = make_duplicate_sources(duplicate_directory)
        duplicate_result = run_case(
            cli,
            Scenario("duplicate_nonlexical", duplicate_paths, DEFAULT_DELIMITER),
            min(4, max(workers)),
            args.repeats,
        )
        result_rows.append(duplicate_result)
        print(json.dumps(duplicate_result, sort_keys=True), flush=True)

        empty_directory = directory / "empty"
        empty_directory.mkdir()
        empty_paths = tuple(empty_directory / f"empty-{index}.jsonl" for index in range(8))
        for path in empty_paths:
            path.write_bytes(b"")
        empty_result = run_case(
            cli,
            Scenario("all_empty", empty_paths, DEFAULT_DELIMITER),
            max(workers),
            args.repeats,
        )
        result_rows.append(empty_result)
        print(json.dumps(empty_result, sort_keys=True), flush=True)

        failure_results = {}
        if not args.skip_failure_probes:
            failure_results = run_failure_probe(cli, edge_paths, DEFAULT_DELIMITER)
            print(
                json.dumps({"type": "failure_probes", "results": failure_results}, sort_keys=True),
                flush=True,
            )

    all_true = all(
        row["determinism"]
        and row["coverage_ok"]
        and row["boundary_ok"]
        and row["exact_once"]
        and row["checksum_ok"]
        for row in result_rows
    )
    summary = {
        "type": "summary",
        "cases": len(result_rows),
        "all_correct": all_true,
        "determinism_cases": sum(row["determinism"] for row in result_rows),
        "coverage_cases": sum(row["coverage_ok"] for row in result_rows),
        "boundary_cases": sum(row["boundary_ok"] for row in result_rows),
        "exact_once_cases": sum(row["exact_once"] for row in result_rows),
        "checksum_cases": sum(row["checksum_ok"] for row in result_rows),
        "failure_probes": failure_results,
    }
    print(json.dumps(summary, sort_keys=True), flush=True)
    return 0 if all_true else 1


if __name__ == "__main__":
    raise SystemExit(main())
