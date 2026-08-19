"""Focused tests for the DataTrove single-file range-backed adoption proof.

These tests exercise the planner manifest and the ``RangeJsonlReader`` against
DataTrove's own ``JsonlReader`` as the oracle. They require the standalone
``mmap-chunker`` CLI (``cargo build --release``), the ``datatrove`` package and
``orjson``; the whole module is skipped when those are unavailable so the core
Rust test suite stays hermetic.

The DataTrove reader opens files in text mode with the locale codec, so on
Windows this module should be run with ``PYTHONUTF8=1`` (see the proof script).
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = REPO_ROOT / "examples"
sys.path.insert(0, str(EXAMPLES))

from datatrove_jsonl_range_reader import (  # noqa: E402
    parse_partition_tsv,
    plan_single_file,
    _split_lines,
)
from datatrove_jsonl_range_reader import RangeJsonlReader  # noqa: E402

try:
    import orjson  # noqa: F401
    from datatrove.executor.local import LocalPipelineExecutor  # noqa: F401
    from datatrove.pipeline.readers.jsonl import JsonlReader  # noqa: F401

    DATATROVE_OK = True
except ImportError:
    DATATROVE_OK = False

CLI = (
    REPO_ROOT
    / "target"
    / "release"
    / ("mmap-chunker.exe" if os.name == "nt" else "mmap-chunker")
)

pytestmark = pytest.mark.skipif(
    not DATATROVE_OK or not CLI.exists(),
    reason="requires datatrove+orjson in the environment and a release CLI build",
)


def _write_jsonl(path: Path, records: list[str], trailing_newline: bool = True) -> int:
    payload = ("\n".join(records) + ("\n" if trailing_newline else "")).encode("utf-8")
    path.write_bytes(payload)
    return len(payload)


def _read_plain(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def test_parse_partition_tsv_roundtrip() -> None:
    stdout = b"0\t0\t10\t10\n1\t10\t20\t10\n"
    rows = parse_partition_tsv(stdout)
    assert rows == [(0, 0, 10, 10), (1, 10, 20, 10)]


def test_parse_partition_tsv_rejects_bad_rows() -> None:
    with pytest.raises(AssertionError):
        parse_partition_tsv(b"0\t0\t10\t9\n")  # length mismatch
    with pytest.raises(AssertionError):
        parse_partition_tsv(b"0\t0\t10\n")  # wrong column count
    with pytest.raises(AssertionError):
        parse_partition_tsv(b"0\tx\t10\t10\n")  # non-numeric


def test_split_lines_matches_text_mode() -> None:
    assert _split_lines("a\nb\n") == ["a", "b"]
    assert _split_lines("a\nb") == ["a", "b"]
    assert _split_lines("a\n\nb\n") == ["a", "", "b"]
    assert _split_lines("a\r\nb\r\n") == ["a", "b"]


def test_plan_is_deterministic_and_contiguous(tmp_path: Path) -> None:
    path = tmp_path / "f.jsonl"
    records = [json.dumps({"text": f"record-{i}", "value": i}) for i in range(1000)]
    _write_jsonl(path, records)
    size = path.stat().st_size

    plan_a = plan_single_file(CLI, path, 8)
    plan_b = plan_single_file(CLI, path, 8)
    assert plan_a.assignments == plan_b.assignments
    assert plan_a.partition_stdout_sha256 == plan_b.partition_stdout_sha256

    assigns = plan_a.assignments
    assert assigns[0].start == 0
    assert assigns[-1].end_exclusive == size
    for a, b in zip(assigns, assigns[1:]):
        assert a.end_exclusive == b.start
    assert all(0 <= a.start <= a.end_exclusive <= size for a in assigns)


def test_plan_boundaries_are_record_aligned(tmp_path: Path) -> None:
    path = tmp_path / "f.jsonl"
    records = [json.dumps({"text": f"record-{i}", "value": i}) for i in range(1000)]
    _write_jsonl(path, records)
    data = path.read_bytes()
    plan = plan_single_file(CLI, path, 8)
    assigns = plan.assignments
    for a in assigns[1:]:
        assert data[a.start - 1] == 0x0A  # start is right after a newline
    for a in assigns[:-1]:
        assert data[a.end_exclusive - 1] == 0x0A  # non-final range ends on newline


def test_plan_empty_file_has_no_assignments(tmp_path: Path) -> None:
    path = tmp_path / "empty.jsonl"
    path.write_bytes(b"")
    plan = plan_single_file(CLI, path, 4)
    assert plan.assignments == ()
    assert plan.file_size == 0


def test_reader_matches_oracle(tmp_path: Path) -> None:
    path = tmp_path / "f.jsonl"
    records = [
        json.dumps({"text": f"record {i} with some payload", "value": i})
        for i in range(500)
    ]
    _write_jsonl(path, records, trailing_newline=False)

    oracle = list(
        JsonlReader(str(tmp_path), glob_pattern="f.jsonl").run(
            data=None, rank=0, world_size=1
        )
    )
    plan = plan_single_file(CLI, path, 4)
    reader = RangeJsonlReader(str(tmp_path), "f.jsonl", plan)
    range_docs = []
    for rank in range(4):
        range_docs.extend(reader.run(data=None, rank=rank, world_size=4))

    assert [d.id for d in range_docs] == [d.id for d in oracle]
    assert [d.text for d in range_docs] == [d.text for d in oracle]
    assert [d.metadata.get("value") for d in range_docs] == [
        d.metadata.get("value") for d in oracle
    ]


def test_reader_records_match_across_workers(tmp_path: Path) -> None:
    path = tmp_path / "f.jsonl"
    records = [json.dumps({"text": f"record-{i}", "value": i}) for i in range(100)]
    _write_jsonl(path, records)
    plan = plan_single_file(CLI, path, 8)
    reader = RangeJsonlReader(str(tmp_path), "f.jsonl", plan)
    seen = []
    for rank in range(8):
        for doc in reader.run(data=None, rank=rank, world_size=8):
            seen.append(doc.metadata["value"])
    assert sorted(seen) == list(range(100))
