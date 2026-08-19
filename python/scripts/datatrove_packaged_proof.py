"""Re-run the DataTrove adoption proof through the INSTALLED Python API.

The goal is not to rediscover the speedup but to prove that packaging did not
destroy it: correctness parity with ordinary DataTrove JsonlReader plus a
bounded performance regression signal at 1/2/4 workers.

Run inside an environment with the wheel installed and the datatrove extra:
    python python/scripts/datatrove_packaged_proof.py --out report.json
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
import tempfile
import time
from pathlib import Path

from mmap_chunker import plan_file
from mmap_chunker.integrations.datatrove import (
    RangeJsonlReader,
    build_range_reader_pipeline,
)

try:
    import orjson
    from datatrove.executor.local import LocalPipelineExecutor
    from datatrove.pipeline.readers.jsonl import JsonlReader
except ImportError as exc:
    raise ImportError(
        "requires datatrove+orjson: pip install mmap-chunker-core[datatrove]"
    ) from exc


WORD_POOL = "alpha beta gamma delta epsilon zeta eta theta iota kappa".split()


def _random_text(rng: random.Random, lo: int, hi: int) -> str:
    length = rng.randint(lo, hi)
    words = []
    used = 0
    while used < length:
        word = rng.choice(WORD_POOL)
        words.append(word)
        used += len(word) + 1
    return " ".join(words)[:length]


def _generate(path: Path, size_mib: int, seed: int) -> tuple[int, int]:
    rng = random.Random(seed)
    target = size_mib * 1024 * 1024
    total = 0
    count = 0
    with open(path, "wb") as fh:
        while total < target:
            record = {"text": _random_text(rng, 200, 400), "value": count}
            payload = orjson.dumps(record) + b"\n"
            fh.write(payload)
            total += len(payload)
            count += 1
    return count, total


def _executor_docs(
    pipeline, tasks: int, workers: int, logging_dir: str
) -> tuple[int, float]:
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
    elapsed = time.perf_counter() - started
    stats_path = Path(logging_dir) / "stats.json"
    if not stats_path.exists():
        raise RuntimeError(f"missing merged stats: {stats_path}")
    with open(stats_path, "r", encoding="utf-8") as fh:
        steps = json.load(fh)
    total = 0
    for step in steps:
        documents = step.get("stats", {}).get("documents", 0)
        if isinstance(documents, dict):
            documents = documents.get("total", 0)
        total += int(documents)
    return total, elapsed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mib", type=int, default=32, help="fixture size in MiB")
    parser.add_argument("--workers", default="1,2,4")
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()
    if not sys.flags.utf8_mode:
        print(
            "note: PYTHONUTF8=1 recommended on Windows for a faithful baseline",
            file=sys.stderr,
        )

    tmp = Path(tempfile.mkdtemp(prefix="dtrovepackaged"))
    logging_root = tmp / "logs"
    logging_root.mkdir()
    fixture_dir = tmp / "data"
    fixture_dir.mkdir()
    path = fixture_dir / "bench.jsonl"
    record_count, file_size = _generate(path, args.mib, 1234)

    workers = [int(w) for w in args.workers.split(",") if w.strip()]

    # Baseline: ordinary DataTrove JsonlReader, single task.
    base_dir = logging_root / "base"
    base_dir.mkdir()
    pipeline = [JsonlReader(str(fixture_dir), glob_pattern="bench.jsonl")]
    base_docs, base_wall = _executor_docs(pipeline, 1, 1, str(base_dir))
    assert base_docs == record_count, f"baseline docs {base_docs} != {record_count}"

    rows = []
    for workers_count in workers:
        plan = plan_file(path, parts=workers_count)
        run_dir = logging_root / f"range_{workers_count}"
        run_dir.mkdir()
        pipe = build_range_reader_pipeline(path, plan)
        docs, wall = _executor_docs(pipe, workers_count, workers_count, str(run_dir))
        speedup = base_wall / wall if wall > 0 else 0.0
        rows.append(
            {
                "workers": workers_count,
                "partitions": plan.actual_partitions,
                "docs": docs,
                "docs_ok": docs == record_count,
                "wall_s": round(wall, 4),
                "speedup_vs_baseline": round(speedup, 3),
            }
        )

    report = {
        "size_mib": args.mib,
        "file_size": file_size,
        "record_count": record_count,
        "baseline": {"workers": 1, "docs": base_docs, "wall_s": round(base_wall, 4)},
        "range_backed": rows,
        "correctness_parity": all(r["docs_ok"] for r in rows),
    }
    payload = json.dumps(report, sort_keys=True, indent=2)
    print(payload)
    if args.out:
        args.out.write_text(payload, encoding="utf-8")
    return 0 if report["correctness_parity"] else 1


if __name__ == "__main__":
    sys.exit(main())
