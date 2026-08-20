#!/usr/bin/env python3
"""Decision-grade adoption proof: one large immutable JSONL file -> DataTrove.

This proof answers, with evidence, whether mmap-chunker-core can turn one
large immutable local JSONL file into useful parallel DataTrove work more
correctly or efficiently than DataTrove's current file-level sharding, with
low enough integration friction to justify the next adoption investment.

Modes:
  correctness  deterministic synthetic fixture matrix: single-task DataTrove
               JsonlReader oracle vs mmap-chunker range-backed reading.
  benchmark    bounded adoption benchmark (smoke/standard, opt-in 1 GiB):
               DataTrove baseline vs range-backed, medians over repeated runs;
               ``--profile skewed`` adds a variable-record workload.
  fsspec       boundary-semantics comparison vs fsspec read_block tiling.
  parallelism  real LocalPipelineExecutor A/B/C structural proof with per-rank
               document counts.
  all          run correctness + fsspec + benchmark (default).

Run from the repository root with the DataTrove environment active and the
release CLI built:

  cargo build --release
  python examples/datatrove_single_file_proof.py --mode all --out report.json
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass, field
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import sys
import tempfile
import time
from typing import Any

import orjson

from datatrove.executor.local import LocalPipelineExecutor
from datatrove.pipeline.readers.jsonl import JsonlReader

from datatrove_jsonl_range_reader import (
    DEFAULT_DELIMITER,
    RangeAssignment,
    SingleFilePlan,
    _split_lines,
    build_range_reader_pipeline,
    plan_single_file,
)

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CLI = (
    ROOT
    / "target"
    / "release"
    / ("mmap-chunker.exe" if os.name == "nt" else "mmap-chunker")
)

WORD_POOL = (
    "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi "
    "omicron pi rho sigma tau upsilon phi chi psi omega terra aqua ignis ventus "
    "celeriter tuto iucunde fortiter sapienter".split()
)

UNICODE_POOL = (
    "汉字漢字日本語αβγδεζηθξΩΨΔΓ日本語українськаfrançaisdeutschöäüßÄÖÜñçêâîôû"
)

PLANNER_WALL = "planner_wall_s"
MANIFEST = "manifest_s"


# ---------------------------------------------------------------------------
# Deterministic synthetic fixtures (no private datasets)
# ---------------------------------------------------------------------------


def _random_text(rng: random.Random, lo: int, hi: int, unicode: bool = False) -> str:
    length = rng.randint(lo, hi)
    if unicode:
        return "".join(rng.choice(UNICODE_POOL) for _ in range(length))
    pool = WORD_POOL
    words: list[str] = []
    used = 0
    while used < length:
        word = rng.choice(pool)
        words.append(word)
        used += len(word) + 1
    return " ".join(words)[:length]


def _write_fixture(
    path: Path,
    records: list[dict[str, Any]],
    *,
    separator: bytes = b"\n",
    trailing_newline: bool = True,
) -> int:
    with open(path, "wb") as fh:
        for i, record in enumerate(records):
            payload = orjson.dumps(record)
            fh.write(payload)
            if separator == b"\n" or i < len(records) - 1 or trailing_newline:
                fh.write(separator)
    return os.path.getsize(path)


def build_correctness_fixtures(root: Path, seed: int = 1234) -> list[dict[str, Any]]:
    """Deterministic fixtures covering the required contract semantics."""
    rng = random.Random(seed)
    fixtures: list[dict[str, Any]] = []

    def make(text: str, value: int, **extra) -> dict[str, Any]:
        return {"text": text, "value": value, **extra}

    # 1. empty file
    fixtures.append({"name": "empty", "parts": [1, 2, 4, 8], "bytes": b""})

    # 2. one record with trailing newline
    one = [make("only record", 1)]
    fixtures.append(
        {
            "name": "one_record",
            "parts": [1, 2, 4],
            "records": one,
        }
    )

    # 3. one record without trailing newline
    fixtures.append(
        {
            "name": "one_record_no_nl",
            "parts": [1, 2, 4],
            "records": one,
            "trailing_newline": False,
        }
    )

    # 4. normal many-record file (LF, trailing newline)
    many = [make(_random_text(rng, 50, 200), i) for i in range(1000)]
    fixtures.append({"name": "many_lf", "parts": [1, 2, 4, 8], "records": many})

    # 5. many records, missing final newline
    fixtures.append(
        {
            "name": "many_no_final_nl",
            "parts": [1, 2, 4, 8],
            "records": many,
            "trailing_newline": False,
        }
    )

    # 6. Unicode content
    uni = [make(_random_text(rng, 20, 120, unicode=True), i) for i in range(200)]
    fixtures.append({"name": "unicode", "parts": [1, 2, 4, 8], "records": uni})

    # 7. varied record sizes (1 byte .. 5 KiB)
    varied = [make(_random_text(rng, 1, 5000), i) for i in range(500)]
    fixtures.append({"name": "varied_sizes", "parts": [1, 2, 4, 8], "records": varied})

    # 8. one giant record crossing several ideal targets
    giant = [
        make(_random_text(rng, 40, 80), 1),
        make("x" * (1024 * 1024), 2),
        make(_random_text(rng, 40, 80), 3),
    ]
    fixtures.append(
        {"name": "giant_record_1mib", "parts": [1, 2, 4, 8], "records": giant}
    )

    # 9. highly skewed record sizes (mostly tiny, a few huge)
    skewed = [make(_random_text(rng, 5, 40), i) for i in range(1500)]
    skewed[7] = make("z" * (200 * 1024), 100000)
    skewed[701] = make("q" * (300 * 1024), 100001)
    skewed[1200] = make("w" * (64 * 1024), 100002)
    fixtures.append({"name": "skewed_sizes", "parts": [1, 2, 4, 8], "records": skewed})

    # 10. requested tasks > actual record-aligned partitions (2 records)
    two = [make("a", 1), make("b", 2)]
    fixtures.append({"name": "tasks_gt_partitions", "parts": [8], "records": two})

    # 11. explicit ids (id_key path) and one malformed JSON line
    with_ids: list[dict[str, Any]] = [
        make(_random_text(rng, 10, 60), i, id=f"doc-{i}") for i in range(300)
    ]
    fixtures.append({"name": "explicit_ids", "parts": [1, 2, 4], "records": with_ids})
    malformed: list[dict[str, Any]] = [
        make(_random_text(rng, 10, 60), i) for i in range(11)
    ]
    malformed_bad_index = 5
    fixtures.append(
        {
            "name": "malformed_json",
            "parts": [1, 2, 4],
            "records": malformed,
            "bad_line_index": malformed_bad_index,
        }
    )

    # 12. CRLF separators (universal-newline parity)
    crlf = [make(_random_text(rng, 20, 100), i) for i in range(500)]
    fixtures.append(
        {
            "name": "crlf",
            "parts": [1, 2, 4],
            "records": crlf,
            "separator": b"\r\n",
        }
    )
    return fixtures


# ---------------------------------------------------------------------------
# Oracle and range-backed collection
# ---------------------------------------------------------------------------


def _canonical_key(document) -> str:
    meta = {k: v for k, v in document.metadata.items() if k != "file_path"}
    payload = orjson.dumps({"t": document.text, "m": meta}, option=orjson.OPT_SORT_KEYS)
    return hashlib.sha256(payload).hexdigest()


def oracle_documents(fixture_dir: Path, name: str) -> list:
    reader = JsonlReader(str(fixture_dir), glob_pattern=name)
    return list(reader.run(data=None, rank=0, world_size=1))


def range_documents(
    cli: Path,
    fixture_dir: Path,
    name: str,
    parts: int,
    delimiter: int = DEFAULT_DELIMITER,
) -> tuple[list, SingleFilePlan]:
    plan = plan_single_file(cli, fixture_dir / name, parts, delimiter)
    reader = build_range_reader_pipeline(str(fixture_dir), name, plan)[0]
    docs: list = []
    for rank in range(parts):
        docs.extend(reader.run(data=None, rank=rank, world_size=parts))
    return docs, plan


# ---------------------------------------------------------------------------
# Correctness matrix
# ---------------------------------------------------------------------------


def _manifest_range_checks(path: Path, plan: SingleFilePlan) -> dict[str, Any]:
    data = path.read_bytes()
    file_size = len(data)
    assigns = plan.assignments
    checks: dict[str, Any] = {}
    checks["in_bounds"] = all(
        0 <= a.start <= a.end_exclusive <= file_size for a in assigns
    )
    checks["lengths_match"] = all(
        a.length == a.end_exclusive - a.start for a in assigns
    )
    contiguous = all(a.end_exclusive == b.start for a, b in zip(assigns, assigns[1:]))
    covers = (not assigns and file_size == 0) or (
        bool(assigns)
        and assigns[0].start == 0
        and assigns[-1].end_exclusive == file_size
    )
    checks["contiguous_no_overlap"] = contiguous
    checks["covers_file"] = covers
    checks["all_owned_in_bounds"] = checks["in_bounds"]
    if not assigns:
        return checks
    starts_after_newline = all(
        a.start == 0 or data[a.start - 1] == 0x0A for a in assigns
    )
    nonfinal_ends_on_newline = all(
        data[a.end_exclusive - 1] == 0x0A for a in assigns[:-1]
    )
    checks["record_aligned_starts"] = starts_after_newline
    checks["record_aligned_ends"] = nonfinal_ends_on_newline
    return checks


def run_correctness_matrix(cli: Path, seed: int = 1234) -> dict[str, Any]:
    tmp = Path(tempfile.mkdtemp(prefix="dtrovecorrect"))
    results: list[dict[str, Any]] = []
    try:
        fixtures = build_correctness_fixtures(tmp, seed)
        for spec in fixtures:
            name = spec["name"]
            path = tmp / f"{name}.jsonl"
            if "bytes" in spec:
                path.write_bytes(spec["bytes"])
            else:
                _write_fixture(
                    path,
                    spec["records"],
                    separator=spec.get("separator", b"\n"),
                    trailing_newline=spec.get("trailing_newline", True),
                )
            expected_skip = 1 if spec.get("bad_line_index") is not None else 0
            if spec.get("bad_line_index") is not None:
                lines = path.read_bytes().split(b"\n")
                if lines and lines[-1] == b"":
                    lines.pop()
                lines[spec["bad_line_index"]] = b'{"text": "unterminated'
                path.write_bytes(b"\n".join(lines) + b"\n")
            oracle = oracle_documents(tmp, f"{name}.jsonl")
            expected_count = (
                0
                if "bytes" in spec and not spec["bytes"]
                else len(spec.get("records", [])) - expected_skip
            )
            oracle_keys = Counter(_canonical_key(d) for d in oracle)
            oracle_ids = Counter(d.id for d in oracle)
            oracle_checksum = hashlib.sha256(
                "".join(_canonical_key(d) for d in oracle).encode()
            ).hexdigest()
            oracle_aggregate = sum(
                d.metadata.get("value", 0)
                for d in oracle
                if isinstance(d.metadata.get("value"), int)
            )

            for parts in spec["parts"]:
                plan_a = plan_single_file(cli, path, parts)
                plan_b = plan_single_file(cli, path, parts)
                deterministic = (
                    plan_a.partition_stdout_sha256 == plan_b.partition_stdout_sha256
                    and plan_a.assignments == plan_b.assignments
                )
                docs, plan = range_documents(cli, tmp, f"{name}.jsonl", parts)
                range_keys = Counter(_canonical_key(d) for d in docs)
                range_ids = Counter(d.id for d in docs)
                range_checksum = hashlib.sha256(
                    "".join(_canonical_key(d) for d in docs).encode()
                ).hexdigest()
                range_aggregate = sum(
                    d.metadata.get("value", 0)
                    for d in docs
                    if isinstance(d.metadata.get("value"), int)
                )
                manifest_checks = _manifest_range_checks(path, plan)
                checks = {
                    "record_count_equal": len(docs) == expected_count
                    and len(docs) == len(oracle),
                    "ids_equal": range_ids == oracle_ids,
                    "canonical_keys_equal": range_keys == oracle_keys,
                    "ordered_text_equal": [d.text for d in docs]
                    == [d.text for d in oracle],
                    "aggregate_equal": range_aggregate == oracle_aggregate,
                    "checksum_equal": range_checksum == oracle_checksum,
                    "no_duplicate_or_missing": range_keys == oracle_keys,
                    **manifest_checks,
                    "deterministic_plan": deterministic,
                }
                results.append(
                    {
                        "fixture": name,
                        "parts": parts,
                        "actual_partitions": len(plan.assignments),
                        "expected_records": expected_count,
                        "actual_records": len(docs),
                        "checks": checks,
                        "pass": all(checks.values()),
                        "file_size": os.path.getsize(path),
                    }
                )
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(1 for r in results if r["pass"])
    return {
        "cases": results,
        "passed": passed,
        "total": len(results),
        "all_pass": passed == len(results),
    }


# ---------------------------------------------------------------------------
# Benchmark
# ---------------------------------------------------------------------------


def generate_benchmark_fixture(
    path: Path, size_mib: int, seed: int, profile: str = "uniform"
) -> int:
    rng = random.Random(seed)
    target = size_mib * 1024 * 1024
    total = 0
    count = 0
    with open(path, "wb") as fh:
        while total < target:
            if profile == "uniform":
                text = _random_text(rng, 200, 400)
            elif profile == "skewed":
                text = _random_text(rng, 8000, 12000) if count % 20 == 0 else _random_text(rng, 20, 80)
            else:
                raise ValueError(f"unknown benchmark profile: {profile}")
            record = {"text": text, "value": count}
            payload = orjson.dumps(record) + b"\n"
            fh.write(payload)
            total += len(payload)
            count += 1
    return count


def _executor_e2e(pipeline, tasks: int, workers: int, logging_dir: str) -> float:
    executor = LocalPipelineExecutor(
        pipeline=pipeline,
        tasks=tasks,
        workers=workers,
        start_method="spawn",
        logging_dir=logging_dir,
        skip_completed=False,
    )
    started = time.perf_counter()
    executor.run()
    return time.perf_counter() - started


def _documents_from_stats(logging_dir: str) -> int:
    stats_path = Path(logging_dir) / "stats.json"
    if not stats_path.exists():
        raise RuntimeError(f"missing merged stats: {stats_path}")
    with open(stats_path, "r", encoding="utf-8") as fh:
        steps = json.load(fh)
    total = 0
    for step in steps:
        stats = step.get("stats", {})
        documents = stats.get("documents", 0)
        if isinstance(documents, dict):
            documents = documents.get("total", 0)
        total += documents
    return int(total)


def _documents_per_rank(logging_dir: str) -> list[int]:
    """Read per-task document totals, keeping empty DataTrove tasks as zero."""
    counts: list[int] = []
    for stats_path in sorted((Path(logging_dir) / "stats").glob("*.json")):
        with open(stats_path, "r", encoding="utf-8") as fh:
            steps = json.load(fh)
        total = 0
        for step in steps:
            documents = step.get("stats", {}).get("documents", 0)
            if isinstance(documents, dict):
                documents = documents.get("total", 0)
            total += int(documents)
        counts.append(total)
    return counts


def run_benchmark(
    cli: Path,
    size_mib: int,
    workers_list: list[int],
    samples: int,
    seed: int,
    logging_root: Path,
    fsspec: bool,
    profile: str = "uniform",
) -> dict[str, Any]:
    tmp = Path(tempfile.mkdtemp(prefix="dtrovebench"))
    fixture_dir = tmp / "data"
    fixture_dir.mkdir()
    name = "bench.jsonl"
    path = fixture_dir / name
    record_count = generate_benchmark_fixture(path, size_mib, seed, profile)
    file_size = os.path.getsize(path)
    file_mib = file_size / (1024 * 1024)

    plan_started = time.perf_counter()
    plan = plan_single_file(cli, path, max(workers_list))
    planning_wall_s = plan.planner_wall_s
    manifest_s = time.perf_counter() - plan_started
    actual_partitions = sum(1 for a in plan.assignments if a.length > 0)

    def baseline_sample() -> tuple[float, int, list[int]]:
        run_dir = logging_root / f"base_{int(time.time() * 1000)}"
        run_dir.mkdir(parents=True, exist_ok=True)
        pipeline = [JsonlReader(str(fixture_dir), glob_pattern=name)]
        elapsed = _executor_e2e(pipeline, tasks=1, workers=1, logging_dir=str(run_dir))
        docs = _documents_from_stats(str(run_dir))
        return elapsed, docs, _documents_per_rank(str(run_dir))

    def range_sample(workers: int, plan) -> tuple[float, int, list[int]]:
        run_dir = logging_root / f"range_{workers}_{int(time.time() * 1000)}"
        run_dir.mkdir(parents=True, exist_ok=True)
        pipeline = build_range_reader_pipeline(str(fixture_dir), name, plan)
        elapsed = _executor_e2e(
            pipeline, tasks=workers, workers=workers, logging_dir=str(run_dir)
        )
        docs = _documents_from_stats(str(run_dir))
        return elapsed, docs, _documents_per_rank(str(run_dir))

    def fsspec_sample(workers: int, plan) -> tuple[float, int, list[int]]:
        from datatrove_fsspec_reader import FsBlockRangeReader

        run_dir = logging_root / f"fsspec_{workers}_{int(time.time() * 1000)}"
        run_dir.mkdir(parents=True, exist_ok=True)
        reader = FsBlockRangeReader(str(fixture_dir), name, plan)
        executor = LocalPipelineExecutor(
            pipeline=[reader],
            tasks=workers,
            workers=workers,
            start_method="spawn",
            logging_dir=str(run_dir),
            skip_completed=False,
        )
        started = time.perf_counter()
        executor.run()
        elapsed = time.perf_counter() - started
        docs = _documents_from_stats(str(run_dir))
        return elapsed, docs, _documents_per_rank(str(run_dir))

    # Pre-plan once per worker count (each rank owns exactly one range).
    plans: dict[int, tuple] = {}
    for workers in workers_list:
        plan_started = time.perf_counter()
        plan = plan_single_file(cli, path, workers)
        plans[workers] = (plan, plan.planner_wall_s, time.perf_counter() - plan_started)

    def run_config(kind: str, workers: int) -> tuple[float, int, list[int]]:
        if kind == "baseline":
            return baseline_sample()
        if kind == "range":
            return range_sample(workers, plans[workers][0])
        return fsspec_sample(workers, plans[workers][0])

    configs = [("baseline", 1)] + [("range", w) for w in workers_list]
    if fsspec:
        configs += [("fsspec", w) for w in workers_list]

    results: dict[tuple[str, int], dict] = {
        c: {"times": [], "docs": [], "ranks": []} for c in configs
    }
    # One discarded warm-up per config (absorbs one-time spawn/import costs).
    for kind, workers in configs:
        run_config(kind, workers)
    # Round-robin interleaving across configs cancels slow machine drift.
    for _ in range(samples):
        for kind, workers in configs:
            elapsed, doc_count, rank_counts = run_config(kind, workers)
            results[(kind, workers)]["times"].append(elapsed)
            results[(kind, workers)]["docs"].append(doc_count)
            results[(kind, workers)]["ranks"].append(rank_counts)

    baseline_times = results[("baseline", 1)]["times"]
    baseline_docs = results[("baseline", 1)]["docs"]
    baseline_ranks = results[("baseline", 1)]["ranks"][0]
    baseline_median = sorted(baseline_times)[len(baseline_times) // 2]

    range_rows = []
    for workers in workers_list:
        plan, planner_wall_s, manifest_s = plans[workers]
        actual_partitions = sum(1 for a in plan.assignments if a.length > 0)
        times = sorted(results[("range", workers)]["times"])
        doc_counts = results[("range", workers)]["docs"]
        rank_counts = results[("range", workers)]["ranks"][0]
        median = times[len(times) // 2]
        speedup = baseline_median / median if median > 0 else float("inf")
        range_rows.append(
            {
                "workers": workers,
                "actual_partitions": actual_partitions,
                "planner_wall_s": planner_wall_s,
                "manifest_s": manifest_s,
                "e2e_median_s": median,
                "e2e_samples_s": times,
                "records_sec": (record_count / median) if median > 0 else 0,
                "mib_sec": (file_mib / median) if median > 0 else 0,
                "docs_processed": doc_counts[0],
                "expected_docs": record_count,
                "docs_ok": all(c == record_count for c in doc_counts),
                "docs_per_rank": rank_counts,
                "speedup_vs_baseline": speedup,
            }
        )

    fsspec_rows = []
    if fsspec:
        for workers in workers_list:
            plan, _planner_wall_s, _manifest_s = plans[workers]
            actual_partitions = sum(1 for a in plan.assignments if a.length > 0)
            times = sorted(results[("fsspec", workers)]["times"])
            doc_counts = results[("fsspec", workers)]["docs"]
            rank_counts = results[("fsspec", workers)]["ranks"][0]
            median = times[len(times) // 2]
            fsspec_rows.append(
                {
                    "workers": workers,
                    "actual_partitions": actual_partitions,
                    "e2e_median_s": median,
                    "e2e_samples_s": times,
                    "records_sec": (record_count / median) if median > 0 else 0,
                    "mib_sec": (file_mib / median) if median > 0 else 0,
                    "docs_processed": doc_counts[0],
                    "expected_docs": record_count,
                    "docs_ok": all(c == record_count for c in doc_counts),
                    "docs_per_rank": rank_counts,
                    "speedup_vs_baseline": baseline_median / median
                    if median > 0
                    else float("inf"),
                }
            )

    shutil.rmtree(tmp, ignore_errors=True)
    return {
        "size_mib": size_mib,
        "profile": profile,
        "file_size": file_size,
        "file_mib": file_mib,
        "record_count": record_count,
        "baseline": {
            "workers": 1,
            "e2e_median_s": baseline_median,
            "e2e_samples_s": baseline_times,
            "docs_processed": baseline_docs[0],
            "expected_docs": record_count,
            "docs_ok": all(c == record_count for c in baseline_docs),
            "docs_per_rank": baseline_ranks,
            "records_sec": record_count / baseline_median if baseline_median > 0 else 0,
            "mib_sec": file_mib / baseline_median if baseline_median > 0 else 0,
        },
        "range_backed": range_rows,
        "fsspec_transport": fsspec_rows,
    }


# ---------------------------------------------------------------------------
# Real LocalPipelineExecutor parallelism proof
# ---------------------------------------------------------------------------


def run_parallelism_proof(
    cli: Path, workers: int, size_mib: int, seed: int
) -> dict[str, Any]:
    """Compare native file sharding and range sharding with real tasks."""
    tmp = Path(tempfile.mkdtemp(prefix="dtroveparallel"))
    try:
        fixture_dir = tmp / "data"
        fixture_dir.mkdir()
        path = fixture_dir / "parallel.jsonl"
        record_count = generate_benchmark_fixture(path, size_mib, seed, "uniform")
        file_size = path.stat().st_size

        def run(pipeline, tasks: int, logging_dir: Path) -> list[int]:
            _executor_e2e(
                pipeline,
                tasks=tasks,
                workers=tasks,
                logging_dir=str(logging_dir),
            )
            return _documents_per_rank(str(logging_dir))

        native_one = run(
            [JsonlReader(str(fixture_dir), glob_pattern=path.name)],
            tasks=1,
            logging_dir=tmp / "native-one",
        )
        native_tasks = run(
            [JsonlReader(str(fixture_dir), glob_pattern=path.name)],
            tasks=workers,
            logging_dir=tmp / "native-tasks",
        )
        plan = plan_single_file(cli, path, workers)
        range_tasks = run(
            build_range_reader_pipeline(str(fixture_dir), path.name, plan),
            tasks=workers,
            logging_dir=tmp / "range-tasks",
        )
        return {
            "size_mib": file_size / (1024 * 1024),
            "file_size": file_size,
            "record_count": record_count,
            "workers": workers,
            "native_one_task": {
                "tasks": 1,
                "workers": 1,
                "documents_per_rank": native_one,
                "nonzero_ranks": sum(count > 0 for count in native_one),
            },
            "native_n_tasks": {
                "tasks": workers,
                "workers": workers,
                "documents_per_rank": native_tasks,
                "nonzero_ranks": sum(count > 0 for count in native_tasks),
                "total_documents": sum(native_tasks),
            },
            "range_n_tasks": {
                "tasks": workers,
                "workers": workers,
                "documents_per_rank": range_tasks,
                "bytes_per_rank": [a.length for a in plan.assignments]
                + [0] * (workers - len(plan.assignments)),
                "nonzero_ranks": sum(count > 0 for count in range_tasks),
                "total_documents": sum(range_tasks),
                "actual_partitions": len(plan.assignments),
                "correct": sum(range_tasks) == record_count
                and sum(count > 0 for count in range_tasks) >= 2,
            },
        }
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# fsspec boundary-semantics comparison
# ---------------------------------------------------------------------------


def fsspec_boundary_demonstration() -> dict[str, Any]:
    """Show the exact difference between naive fsspec tiling and the manifest.

    Uses a fixed record-size layout (24 records, 4-way arithmetic tiling) where
    fsspec ``read_block`` boundary alignment double-reads record 11 — a
    duplicate that mmap-chunker's record-aligned manifest excludes by
    construction. The layout was found deterministically by fuzzing 300 random
    layouts (seed 7); this run reproduces the failure.
    """
    import fsspec

    sizes = [
        5,
        2000,
        100,
        100,
        100,
        2000,
        5,
        2000,
        10,
        10,
        10,
        5,
        10,
        500,
        100,
        2000,
        10,
        500,
        500,
        100,
        2000,
        30,
        10,
        500,
    ]
    workers = 4
    tmp = Path(tempfile.mkdtemp(prefix="dtrovefs"))
    try:
        path = tmp / "tile.jsonl"
        lines = [
            orjson.dumps({"text": f"r{i}" + "x" * sizes[i], "value": i}).decode()
            for i in range(len(sizes))
        ]
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        size = os.path.getsize(path)
        fs = fsspec.filesystem("file")
        expected = set(range(len(sizes)))
        blocks = []
        covered: Counter = Counter()
        for k in range(workers):
            offset = (k * size) // workers
            length = size // workers + 1
            block = fs.read_block(str(path), offset, length, delimiter=b"\n")
            record_ids = []
            for raw in block.split(b"\n"):
                if not raw:
                    continue
                try:
                    value = orjson.loads(raw)["value"]
                    record_ids.append(value)
                    covered[value] += 1
                except orjson.JSONDecodeError:
                    record_ids.append(-1)
            blocks.append(
                {
                    "worker": k,
                    "requested_offset": offset,
                    "requested_length": length,
                    "returned_bytes": len(block),
                    "record_values": record_ids,
                }
            )
        covered_set = set(covered)
        missing = sorted(expected - covered_set)
        duplicates = sorted({k: v for k, v in covered.items() if v > 1})

        # read_block aligns both ends FORWARD (seek_delimiter), so a mid-file
        # request skips the first record that starts at the requested offset.
        # Demonstrate this directly: request a block at the start of record 5.
        skip_path = tmp / "skip.jsonl"
        skip_lines = [
            orjson.dumps({"text": f"rec-{i}", "value": i}).decode() for i in range(10)
        ]
        skip_path.write_text("\n".join(skip_lines) + "\n", encoding="utf-8")
        skip_size = os.path.getsize(skip_path)
        offsets = []
        with open(skip_path, "rb") as fh:
            pos = 0
            for _ in range(10):
                offsets.append(pos)
                line = fh.readline()
                pos += len(line)
        rec5_offset = offsets[5]
        block = fs.read_block(
            str(skip_path), rec5_offset, skip_size - rec5_offset, delimiter=b"\n"
        )
        skip_values = []
        for raw in block.split(b"\n"):
            if raw:
                skip_values.append(orjson.loads(raw)["value"])
        forward_skip = {
            "record_5_offset": rec5_offset,
            "requested_length": skip_size - rec5_offset,
            "returned_first_record": skip_values[0] if skip_values else None,
            "skips_first_record_at_offset": skip_values[0] != 5,
            "returned_records": skip_values,
        }

        return {
            "file_size": size,
            "workers": workers,
            "record_sizes": sizes,
            "blocks": blocks,
            "missing_records": missing,
            "duplicate_records": duplicates,
            "naive_tiling_gaps": bool(missing),
            "naive_tiling_duplicates": bool(duplicates),
            "read_block_forward_skip": forward_skip,
        }
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cli",
        type=Path,
        default=DEFAULT_CLI,
        help="standalone mmap-chunker executable",
    )
    parser.add_argument(
        "--mode",
        choices=["correctness", "benchmark", "fsspec", "parallelism", "all"],
        default="all",
    )
    parser.add_argument(
        "--workers",
        type=str,
        default="1,2,4,8",
        help="comma-separated worker counts for the benchmark",
    )
    parser.add_argument("--smoke-mib", type=int, default=32)
    parser.add_argument("--standard-mib", type=int, default=256)
    parser.add_argument(
        "--gib", type=float, default=0.0, help="opt-in extra size in GiB (0 disables)"
    )
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--seed", type=int, default=1234)
    parser.add_argument(
        "--profile",
        choices=["uniform", "skewed"],
        default="uniform",
        help="benchmark record-size profile",
    )
    parser.add_argument("--out", type=Path, default=None, help="write JSON report here")
    return parser


def main() -> int:
    args = build_parser().parse_args()
    if not sys.flags.utf8_mode:
        print(
            "note: Python UTF-8 mode is off; DataTrove's JsonlReader opens files in "
            "text mode with the locale codec, so the unicode correctness case will "
            "fail on non-UTF-8 locales (e.g. cp1252 on Windows). Run with "
            "PYTHONUTF8=1 for a faithful UTF-8 baseline.",
            file=sys.stderr,
        )
    cli = args.cli
    if not cli.exists():
        print(
            f"error: CLI not found at {cli} (run `cargo build --release` first)",
            file=sys.stderr,
        )
        return 2
    workers = [int(w) for w in args.workers.split(",") if w.strip()]
    logging_root = Path(tempfile.mkdtemp(prefix="dtrovelogs"))
    report: dict[str, Any] = {
        "datatrove_version": _datatrove_version(),
        "datatrove_commit": _datatrove_commit(),
        "mmap_chunker_cli": str(cli),
        "seed": args.seed,
    }
    exit_code = 0

    if args.mode in ("correctness", "all"):
        matrix = run_correctness_matrix(cli, args.seed)
        report["correctness"] = matrix
        if not matrix["all_pass"]:
            exit_code = 1
        print(f"correctness: {matrix['passed']}/{matrix['total']} cases passed")

    if args.mode in ("fsspec", "all"):
        report["fsspec_boundary"] = fsspec_boundary_demonstration()
        print("fsspec boundary demonstration complete")

    if args.mode in ("benchmark", "all"):
        sizes = [(args.smoke_mib, "smoke")]
        if args.standard_mib > args.smoke_mib:
            sizes.append((args.standard_mib, "standard"))
        if args.gib > 0:
            sizes.append((int(args.gib * 1024), "opt-in-gib"))
        benchmarks = []
        for size_mib, label in sizes:
            bench = run_benchmark(
                cli,
                size_mib,
                workers,
                args.samples,
                args.seed,
                logging_root,
                fsspec=args.mode in ("all", "benchmark"),
                profile=args.profile,
            )
            bench["label"] = label
            benchmarks.append(bench)
            print(
                f"benchmark {label}: {size_mib} MiB, {bench['record_count']} records, "
                f"baseline {bench['baseline']['e2e_median_s']:.3f}s"
            )
            for row in bench["range_backed"]:
                print(
                    f"  range workers={row['workers']} e2e={row['e2e_median_s']:.3f}s "
                    f"speedup={row['speedup_vs_baseline']:.2f}x docs_ok={row['docs_ok']}"
                )
            for row in bench["fsspec_transport"]:
                print(
                    f"  fsspec workers={row['workers']} e2e={row['e2e_median_s']:.3f}s "
                    f"speedup={row['speedup_vs_baseline']:.2f}x docs_ok={row['docs_ok']}"
                )
        report["benchmarks"] = benchmarks

    if args.mode in ("parallelism", "all"):
        parallel = run_parallelism_proof(
            cli, workers=max(workers), size_mib=args.smoke_mib, seed=args.seed
        )
        report["parallelism"] = parallel
        print(
            "parallelism: native tasks nonzero ranks="
            f"{parallel['native_n_tasks']['nonzero_ranks']}, range tasks nonzero "
            f"ranks={parallel['range_n_tasks']['nonzero_ranks']}"
        )

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(
            orjson.dumps(report, option=orjson.OPT_INDENT_2).decode(), encoding="utf-8"
        )
        print(f"report written to {args.out}")
    shutil.rmtree(logging_root, ignore_errors=True)
    return exit_code


def _datatrove_version() -> str:
    import importlib.metadata

    try:
        return importlib.metadata.version("datatrove")
    except importlib.metadata.PackageNotFoundError:
        return "unknown"


def _datatrove_commit() -> str:
    try:
        import datatrove

        package_dir = Path(datatrove.__file__).resolve().parent
        repo = package_dir
        while repo != repo.parent and not (repo / ".git").exists():
            repo = repo.parent
        git_dir = repo / ".git"
        if not git_dir.exists():
            return "unknown"
        head_file = git_dir / "HEAD"
        if not head_file.exists():
            return "unknown"
        ref = head_file.read_text(encoding="utf-8").strip()
        if ref.startswith("ref:"):
            ref_path = git_dir / ref.split(":", 1)[1].strip().replace("/", os.sep)
            if ref_path.exists():
                return ref_path.read_text(encoding="utf-8").strip()[:12]
        return ref[:12]
    except Exception:
        return "unknown"


if __name__ == "__main__":
    sys.exit(main())
