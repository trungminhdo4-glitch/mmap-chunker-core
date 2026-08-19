"""DataTrove integration tests through the installed mmap_chunker package.

Requires the ``[datatrove]`` extra (``pip install mmap-chunker-core[datatrove]``);
skipped otherwise. On Windows, run with ``PYTHONUTF8=1`` because DataTrove's
JsonlReader opens files in text mode with the locale codec.
"""

from __future__ import annotations

import json
import os
import random
import sys
from pathlib import Path
import tempfile
import time

import pytest

try:
    import mmap_chunker  # noqa: F401

    from mmap_chunker.integrations.datatrove import (
        RangeJsonlReader,
        build_range_reader_pipeline,
    )
    from mmap_chunker import plan_file  # noqa: F401

    import orjson  # noqa: F401
    from datatrove.executor.local import LocalPipelineExecutor  # noqa: F401
    from datatrove.pipeline.readers.jsonl import JsonlReader  # noqa: F401

    DATATROVE_OK = True
except ImportError:
    DATATROVE_OK = False

_REPO = Path(__file__).resolve().parents[2]
_PYTHON_SRC = _REPO / "python"
if not DATATROVE_OK:
    # Base package must still be importable (source-tree fallback) so tests
    # for it can run even without the datatrove extra.
    try:
        import mmap_chunker  # noqa: F401

        from mmap_chunker import plan_file  # noqa: F401
    except ImportError:
        sys.path.insert(0, str(_PYTHON_SRC))
        from mmap_chunker import plan_file  # noqa: E402

pytestmark = pytest.mark.skipif(
    not DATATROVE_OK,
    reason="requires datatrove+orjson (pip install mmap-chunker-core[datatrove])",
)

_REPO = Path(__file__).resolve().parents[2]


def _write_jsonl(path: Path, records: list[dict], trailing: bool = True) -> int:
    with open(path, "wb") as fh:
        for i, record in enumerate(records):
            fh.write(orjson.dumps(record))
            fh.write(b"\n")
    if not trailing:
        data = path.read_bytes()
        path.write_bytes(data[:-1])
    return path.stat().st_size


def _read_plain(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _range_docs(reader, parts: int) -> list:
    docs = []
    for rank in range(parts):
        docs.extend(reader.run(data=None, rank=rank, world_size=parts))
    return docs


def test_core_package_imports_without_datatrove() -> None:
    # The base package must be importable without datatrove installed.
    import mmap_chunker

    assert mmap_chunker.__version__
    assert mmap_chunker.plan_file


def test_reader_matches_oracle(tmp_path: Path) -> None:
    records = [
        {"text": f"record {i} with some payload", "value": i, "id": f"doc-{i}"}
        for i in range(500)
    ]
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, records, trailing=False)

    oracle = list(
        JsonlReader(str(tmp_path), glob_pattern="f.jsonl").run(
            data=None, rank=0, world_size=1
        )
    )
    plan = plan_file(path, parts=4)
    reader = RangeJsonlReader(path, plan)
    docs = _range_docs(reader, 4)

    assert [d.id for d in docs] == [d.id for d in oracle]
    assert [d.text for d in docs] == [d.text for d in oracle]
    assert [d.metadata.get("value") for d in docs] == [
        d.metadata.get("value") for d in oracle
    ]


def test_records_match_across_workers(tmp_path: Path) -> None:
    records = [{"text": f"record-{i}", "value": i} for i in range(100)]
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, records)
    plan = plan_file(path, parts=8)
    reader = RangeJsonlReader(path, plan)
    seen = [d.metadata["value"] for d in _range_docs(reader, 8)]
    assert sorted(seen) == list(range(100))


def test_empty_file_no_assignments(tmp_path: Path) -> None:
    path = tmp_path / "empty.jsonl"
    path.write_bytes(b"")
    plan = plan_file(path, parts=4)
    assert plan.ranges == ()
    reader = RangeJsonlReader(path, plan)
    assert list(_range_docs(reader, 4)) == []


def test_pipeline_builder(tmp_path: Path) -> None:
    records = [{"text": f"r{i}", "value": i} for i in range(50)]
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, records)
    plan = plan_file(path, parts=2)
    pipeline = build_range_reader_pipeline(path, plan)
    assert len(pipeline) == 1
    docs = _range_docs(pipeline[0], 2)
    assert len(docs) == 50


def test_plan_path_must_match(tmp_path: Path) -> None:
    a = tmp_path / "a.jsonl"
    b = tmp_path / "b.jsonl"
    _write_jsonl(a, [{"text": "a"}])
    _write_jsonl(b, [{"text": "b"}])
    plan = plan_file(a, parts=1)
    with pytest.raises(ValueError):
        RangeJsonlReader(b, plan)


def test_worker_smoke_performance(tmp_path: Path) -> None:
    """Bounded regression signal: range-backed must not be slower than baseline.

    Uses a standard-size fixture and reports relative performance without
    reproducing exact historical benchmark numbers.
    """
    records = [
        {
            "text": "word " * 40 + str(i),
            "value": i,
        }
        for i in range(5000)
    ]
    path = tmp_path / "bench.jsonl"
    _write_jsonl(path, records)

    def baseline_wall() -> tuple[float, int]:
        start = time.perf_counter()
        docs = list(
            JsonlReader(str(tmp_path), glob_pattern="bench.jsonl").run(
                data=None, rank=0, world_size=1
            )
        )
        return time.perf_counter() - start, len(docs)

    def range_wall(parts: int) -> tuple[float, int]:
        reader = RangeJsonlReader(path, plan_file(path, parts=parts))
        start = time.perf_counter()
        docs = _range_docs(reader, parts)
        return time.perf_counter() - start, len(docs)

    base_time, base_count = baseline_wall()
    times = {}
    counts = {}
    for workers in (1, 2, 4):
        wall, count = range_wall(workers)
        times[workers] = wall
        counts[workers] = count
        assert count == base_count == len(records)

    report = {
        "baseline_workers": 1,
        "baseline_wall_s": round(base_time, 4),
        "range_workers": times,
        "range_counts_ok": all(c == base_count for c in counts.values()),
        "records": base_count,
        "speedup_vs_baseline": {
            k: round(base_time / v, 3) if v > 0 else 0.0 for k, v in times.items()
        },
    }
    print(f"datatrove_smoke={json.dumps(report, sort_keys=True)}")
