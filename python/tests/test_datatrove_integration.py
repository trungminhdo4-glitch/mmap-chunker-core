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
import tracemalloc

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


def _rank_document_counts(logging_dir: Path) -> list[int]:
    counts = []
    for stats_path in sorted((logging_dir / "stats").glob("*.json")):
        payload = json.loads(stats_path.read_text(encoding="utf-8"))
        reader_stats = next(
            (step["stats"] for step in payload if "documents" in step["stats"]),
            {},
        )
        documents = reader_stats.get("documents", 0)
        counts.append(documents["total"] if isinstance(documents, dict) else documents)
    return counts


def _aggregate_reader_stats(logging_dir: Path) -> dict[str, int]:
    totals = {"input_files": 0, "documents": 0, "doc_len": 0}
    for stats_path in sorted((logging_dir / "stats").glob("*.json")):
        payload = json.loads(stats_path.read_text(encoding="utf-8"))
        for step in payload:
            stats = step.get("stats", {})
            for key in totals:
                value = stats.get(key, 0)
                totals[key] += value.get("total", 0) if isinstance(value, dict) else value
    return totals


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


@pytest.mark.parametrize(
    ("kwargs", "expected_ids"),
    [
        ({"limit": 0}, []),
        ({"limit": 1}, ["f.jsonl/0"]),
        ({"limit": 5}, [f"f.jsonl/{i}" for i in range(5)]),
        ({"skip": 2}, [f"f.jsonl/{i}" for i in range(2, 10)]),
        (
            {"skip": 2, "limit": 5},
            [f"f.jsonl/{i}" for i in range(2, 7)],
        ),
    ],
)
def test_limit_and_skip_match_datatrove_single_task(
    tmp_path: Path, kwargs: dict, expected_ids: list[str]
) -> None:
    records = [{"text": f"record-{i}", "value": i} for i in range(10)]
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, records)

    oracle = list(
        JsonlReader(str(tmp_path), glob_pattern="f.jsonl", **kwargs).run(
            data=None, rank=0, world_size=1
        )
    )
    plan = plan_file(path, parts=1)
    candidate = list(RangeJsonlReader(path, plan, **kwargs).run(data=None, rank=0, world_size=1))

    assert [document.id for document in oracle] == expected_ids
    assert [document.id for document in candidate] == expected_ids


def test_plan_world_size_mismatch_is_explicit(tmp_path: Path) -> None:
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, [{"text": f"record-{i}"} for i in range(10)])
    plan = plan_file(path, parts=4)
    reader = RangeJsonlReader(path, plan)

    with pytest.raises(ValueError, match="world_size must equal plan.requested_parts"):
        list(reader.run(data=None, rank=0, world_size=2))


def test_adapter_metadata_keys_and_custom_adapter_match_oracle(tmp_path: Path) -> None:
    records = [
        {
            "body": f"body-{i}",
            "custom_id": f"custom-{i}",
            "metadata": {"source": "fixture", "row": i},
            "value": i,
        }
        for i in range(4)
    ]
    path = tmp_path / "options.jsonl"
    _write_jsonl(path, records)
    kwargs = {
        "text_key": "body",
        "id_key": "custom_id",
        "default_metadata": {"batch": "parity"},
        "add_file_path": False,
    }
    oracle = list(
        JsonlReader(str(tmp_path), glob_pattern="options.jsonl", **kwargs).run(
            data=None, rank=0, world_size=1
        )
    )
    plan = plan_file(path, parts=1)
    candidate = list(
        RangeJsonlReader(path, plan, **kwargs).run(data=None, rank=0, world_size=1)
    )

    assert [(d.id, d.text, d.metadata) for d in candidate] == [
        (d.id, d.text, d.metadata) for d in oracle
    ]
    assert all("file_path" not in d.metadata for d in candidate)

    def adapter(self, data, path, id_in_file):
        return {
            "text": data["body"].upper(),
            "id": f"adapted-{id_in_file}",
            "metadata": {"adapter_path": path},
        }

    oracle_custom = list(
        JsonlReader(
            str(tmp_path), glob_pattern="options.jsonl", adapter=adapter
        ).run(data=None, rank=0, world_size=1)
    )
    candidate_custom = list(
        RangeJsonlReader(path, plan, adapter=adapter).run(
            data=None, rank=0, world_size=1
        )
    )
    assert [(d.id, d.text, d.metadata) for d in candidate_custom] == [
        (d.id, d.text, d.metadata) for d in oracle_custom
    ]


def test_range_reader_does_not_materialize_the_whole_range(tmp_path: Path) -> None:
    path = tmp_path / "large.jsonl"
    record = {"text": "x" * 100, "value": 0}
    _write_jsonl(path, [record] * 40_000)
    plan = plan_file(path, parts=1)
    reader = RangeJsonlReader(path, plan)

    tracemalloc.start()
    first = next(reader._read_range(plan.ranges[0]))
    _current, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()

    assert first.text == record["text"]
    assert path.stat().st_size > 4 * 1024 * 1024
    assert peak < 2 * 1024 * 1024


def test_record_offset_manifest_uses_bounded_reads(tmp_path: Path) -> None:
    path = tmp_path / "offsets.jsonl"
    record = {"text": "y" * 100, "value": 0}
    _write_jsonl(path, [record] * 120_000)
    plan = plan_file(path, parts=4)

    tracemalloc.start()
    reader = RangeJsonlReader(path, plan)
    _current, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()

    assert reader._offsets[0] == 0
    assert len(reader._offsets) == 4
    assert all(a < b for a, b in zip(reader._offsets, reader._offsets[1:]))
    assert path.stat().st_size > 12 * 1024 * 1024
    assert peak < 8 * 1024 * 1024


def test_local_executor_really_spreads_one_file_across_ranks(tmp_path: Path) -> None:
    records = [{"text": f"record-{i}", "value": i} for i in range(100)]
    path = tmp_path / "f.jsonl"
    _write_jsonl(path, records)

    baseline_log = tmp_path / "baseline-log"
    LocalPipelineExecutor(
        pipeline=[JsonlReader(str(tmp_path), glob_pattern="f.jsonl")],
        tasks=4,
        workers=4,
        start_method="spawn",
        logging_dir=str(baseline_log),
        skip_completed=False,
    ).run()
    baseline_counts = _rank_document_counts(baseline_log)

    plan = plan_file(path, parts=4)
    range_log = tmp_path / "range-log"
    LocalPipelineExecutor(
        pipeline=[RangeJsonlReader(path, plan)],
        tasks=4,
        workers=4,
        start_method="spawn",
        logging_dir=str(range_log),
        skip_completed=False,
    ).run()
    range_counts = _rank_document_counts(range_log)
    baseline_stats = _aggregate_reader_stats(baseline_log)
    range_stats = _aggregate_reader_stats(range_log)

    assert sum(baseline_counts) == len(records)
    assert sum(count > 0 for count in baseline_counts) == 1
    assert sum(range_counts) == len(records)
    assert sum(count > 0 for count in range_counts) >= 2
    assert range_stats == baseline_stats


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
