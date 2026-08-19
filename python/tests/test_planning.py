"""Correctness matrix and public-API tests for mmap_chunker.plan_file.

Runs against the installed package (or the repository source tree). Each case
verifies exact file coverage, no overlaps, no gaps, correct boundaries,
record alignment, determinism, and requested/actual range semantics. A CLI
parity check compares the Python API against the standalone ``mmap-chunker
partition`` output on the same fixtures.
"""

from __future__ import annotations

import json
import os
import random
import subprocess
import sys
from pathlib import Path

import pytest

_REPO = Path(__file__).resolve().parents[2]
_PYTHON_SRC = _REPO / "python"

# Prefer the installed package; fall back to the repository source tree.
try:
    import mmap_chunker  # noqa: F401

    _FROM_SOURCE = False
except ImportError:
    sys.path.insert(0, str(_PYTHON_SRC))
    import mmap_chunker  # noqa: F401

    _FROM_SOURCE = True

from mmap_chunker import Plan, PlanningError, Range, plan_file  # noqa: E402

CLI = (
    _REPO
    / "target"
    / "release"
    / ("mmap-chunker.exe" if os.name == "nt" else "mmap-chunker")
)

WORD_POOL = (
    "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi "
    "omicron pi rho sigma tau upsilon phi chi psi omega terra aqua ignis ventus "
    "celeriter tuto iucunde fortiter sapienter".split()
)
UNICODE_POOL = "汉字漢字日本語αβγδεζηθξΩΨΔΓукраїнськаfrançaisdeutschöäüßÄÖÜñçêâîôû"


def _random_text(rng: random.Random, lo: int, hi: int, unicode: bool = False) -> str:
    length = rng.randint(lo, hi)
    if unicode:
        return "".join(rng.choice(UNICODE_POOL) for _ in range(length))
    words = []
    used = 0
    while used < length:
        word = rng.choice(WORD_POOL)
        words.append(word)
        used += len(word) + 1
    return " ".join(words)[:length]


def _write_fixture(path: Path, records: list[dict], trailing: bool = True) -> int:
    with open(path, "wb") as fh:
        for i, record in enumerate(records):
            fh.write(json.dumps(record).encode("utf-8"))
            fh.write(b"\n")
    if not trailing:
        data = path.read_bytes()
        path.write_bytes(data[:-1])
    return path.stat().st_size


def _verify_plan(path: Path, plan: Plan, delimiter: int, parts: int) -> None:
    data = path.read_bytes()
    file_size = len(data)
    ranges = plan.ranges
    assert plan.file_size == file_size
    assert plan.requested_parts == parts
    assert plan.delimiter == delimiter
    if file_size == 0:
        assert ranges == ()
        return
    assert len(ranges) >= 1
    assert ranges[0].start == 0
    assert ranges[-1].end == file_size
    for r in ranges:
        assert 0 <= r.start <= r.end <= file_size
        assert r.length == r.end - r.start
        assert r.index == ranges.index(r)
    for a, b in zip(ranges, ranges[1:]):
        assert a.end == b.start, "no gaps, no overlaps, monotonic"
    for r in ranges[1:]:
        assert data[r.start - 1] == delimiter
    for r in ranges[:-1]:
        assert data[r.end - 1] == delimiter
    covered = sum(r.length for r in ranges)
    assert covered == file_size, "complete coverage"


def test_version_and_abi() -> None:
    assert mmap_chunker.__version__ == "0.2.4"
    assert mmap_chunker.abi_version() == 0x0001_0003
    caps = mmap_chunker.capabilities()
    assert caps & (1 << 4)  # RECORD_PARTITIONING


def test_plan_file_returns_immutable_objects() -> None:
    p = Path(__import__("tempfile").mkdtemp()) / "f.jsonl"
    _write_fixture(p, [{"text": "x"}] * 10)
    plan = plan_file(p, parts=2)
    assert isinstance(plan, Plan)
    assert isinstance(plan.ranges, tuple)
    assert all(isinstance(r, Range) for r in plan.ranges)
    with pytest.raises(AttributeError):
        plan.ranges = ()  # type: ignore[misc]
    with pytest.raises(AttributeError):
        plan.ranges[0].start = 5  # type: ignore[misc]


def test_range_lengths_positive() -> None:
    p = Path(__import__("tempfile").mkdtemp()) / "f.jsonl"
    _write_fixture(p, [{"text": "x"}] * 10)
    plan = plan_file(p, parts=4)
    assert all(r.length > 0 for r in plan.ranges)


@pytest.mark.parametrize("delimiter", [b"\n", 10, 0x0A])
def test_delimiter_representations(delimiter) -> None:
    p = Path(__import__("tempfile").mkdtemp()) / "f.jsonl"
    _write_fixture(p, [{"text": "x"}] * 20)
    plan = plan_file(p, parts=4, delimiter=delimiter)
    _verify_plan(p, plan, 10, 4)


@pytest.mark.parametrize("parts", [1, 2, 4, 8])
def test_parts_matrix(tmp_path: Path, parts: int) -> None:
    records = [
        {"text": _random_text(random.Random(i), 50, 200), "value": i}
        for i in range(1000)
    ]
    p = tmp_path / "f.jsonl"
    _write_fixture(p, records)
    plan = plan_file(p, parts=parts)
    _verify_plan(p, plan, 10, parts)
    assert plan.actual_partitions == parts


def test_empty_file(tmp_path: Path) -> None:
    p = tmp_path / "empty.jsonl"
    p.write_bytes(b"")
    plan = plan_file(p, parts=4)
    assert plan.ranges == ()
    assert plan.file_size == 0
    assert plan.actual_partitions == 0


def test_one_record_with_and_without_trailing_newline(tmp_path: Path) -> None:
    for name, trailing in (("one_nl", True), ("one_no_nl", False)):
        p = tmp_path / f"{name}.jsonl"
        _write_fixture(p, [{"text": "only record", "value": 1}], trailing=trailing)
        for parts in (1, 2, 4):
            plan = plan_file(p, parts=parts)
            _verify_plan(p, plan, 10, parts)
            assert plan.actual_partitions == 1


def test_missing_final_newline(tmp_path: Path) -> None:
    records = [{"text": f"r{i}", "value": i} for i in range(100)]
    p = tmp_path / "no_final_nl.jsonl"
    _write_fixture(p, records, trailing=False)
    data = p.read_bytes()
    assert data[-1] != 0x0A
    for parts in (1, 2, 4, 8):
        plan = plan_file(p, parts=parts)
        _verify_plan(p, plan, 10, parts)


def test_unicode_path_and_payload(tmp_path: Path) -> None:
    p = tmp_path / "データ-Δ-ümlaut.jsonl"
    records = [
        {"text": _random_text(random.Random(i), 20, 80, unicode=True), "value": i}
        for i in range(100)
    ]
    _write_fixture(p, records)
    for parts in (1, 2, 4):
        plan = plan_file(p, parts=parts)
        _verify_plan(p, plan, 10, parts)


def test_giant_record(tmp_path: Path) -> None:
    records = [
        {"text": _random_text(random.Random(1), 40, 80), "value": 1},
        {"text": "x" * (1024 * 1024), "value": 2},
        {"text": _random_text(random.Random(2), 40, 80), "value": 3},
    ]
    p = tmp_path / "giant.jsonl"
    _write_fixture(p, records)
    for parts in (1, 2, 4, 8):
        plan = plan_file(p, parts=parts)
        _verify_plan(p, plan, 10, parts)
        assert plan.actual_partitions <= 3  # giant record collapses boundaries


def test_skewed_records(tmp_path: Path) -> None:
    records = [
        {"text": _random_text(random.Random(i), 5, 40), "value": i} for i in range(1500)
    ]
    records[7] = {"text": "z" * (200 * 1024), "value": 100000}
    records[701] = {"text": "q" * (300 * 1024), "value": 100001}
    p = tmp_path / "skewed.jsonl"
    _write_fixture(p, records)
    for parts in (1, 2, 4, 8):
        plan = plan_file(p, parts=parts)
        _verify_plan(p, plan, 10, parts)


def test_tasks_greater_than_records(tmp_path: Path) -> None:
    records = [{"text": "a", "value": 1}, {"text": "b", "value": 2}]
    p = tmp_path / "two.jsonl"
    _write_fixture(p, records)
    plan = plan_file(p, parts=8)
    _verify_plan(p, plan, 10, 8)
    assert plan.actual_partitions == 2


def test_custom_delimiter_byte(tmp_path: Path) -> None:
    p = tmp_path / "csv.bin"
    p.write_bytes(b"aa,bb,cc,dd,ee,ff,gg,hh,")
    plan = plan_file(p, parts=4, delimiter=ord(","))
    _verify_plan(p, plan, ord(","), 4)


def test_deterministic_repeated_plan(tmp_path: Path) -> None:
    records = [{"text": f"r{i}", "value": i} for i in range(500)]
    p = tmp_path / "f.jsonl"
    _write_fixture(p, records)
    first = plan_file(p, parts=8)
    for _ in range(5):
        again = plan_file(p, parts=8)
        assert again.ranges == first.ranges
        assert again.file_size == first.file_size


def test_plan_does_not_keep_handle_open(tmp_path: Path) -> None:
    records = [{"text": f"r{i}", "value": i} for i in range(100)]
    p = tmp_path / "f.jsonl"
    _write_fixture(p, records)
    plan = plan_file(p, parts=4)
    # The file must be freely removable/renameable after planning: no live mmap.
    renamed = tmp_path / "renamed.jsonl"
    os.replace(p, renamed)
    plan2 = plan_file(renamed, parts=4)
    assert plan2.ranges[0].start == 0


def test_path_like_input(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    _write_fixture(p, [{"text": "x"}] * 10)
    plan = plan_file(p, parts=2)
    assert plan.path == str(p.resolve())


def test_reject_invalid_inputs(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    _write_fixture(p, [{"text": "x"}] * 10)
    with pytest.raises(ValueError):
        plan_file(p, parts=0)
    with pytest.raises(ValueError):
        plan_file(p, parts=-1)
    with pytest.raises(TypeError):
        plan_file(p, parts="8")  # type: ignore[arg-type]
    with pytest.raises(ValueError):
        plan_file(p, parts=4, delimiter=b"\r\n")  # multi-byte delimiter
    with pytest.raises(TypeError):
        plan_file(p, parts=4, delimiter="\n")  # type: ignore[arg-type]
    with pytest.raises(ValueError):
        plan_file(p, parts=4, delimiter=256)
    with pytest.raises(ValueError):
        plan_file(p, parts=4, delimiter=-1)
    with pytest.raises(ValueError):
        plan_file(p, parts=4, delimiter=True)  # type: ignore[arg-type]
    with pytest.raises(FileNotFoundError):
        plan_file(tmp_path / "missing.jsonl", parts=4)
    with pytest.raises(IsADirectoryError):
        plan_file(tmp_path, parts=4)
    with pytest.raises(ValueError):
        plan_file(tmp_path / "nul\x00path.jsonl", parts=4)
    with pytest.raises(TypeError):
        plan_file(12345, parts=4)  # type: ignore[arg-type]


def test_fixture_reconstruction_matches_oracle(tmp_path: Path) -> None:
    """Every byte of the file must appear exactly once across the ranges."""
    records = [
        {"text": _random_text(random.Random(i), 10, 300), "value": i}
        for i in range(200)
    ]
    p = tmp_path / "f.jsonl"
    _write_fixture(p, records)
    expected = p.read_bytes()
    for parts in (1, 2, 4, 8):
        plan = plan_file(p, parts=parts)
        chunks = []
        with open(p, "rb") as fh:
            for r in plan.ranges:
                fh.seek(r.start)
                chunks.append(fh.read(r.length))
        assert b"".join(chunks) == expected


@pytest.mark.skipif(not CLI.exists(), reason="release CLI not built")
def test_cli_parity(tmp_path: Path) -> None:
    records = [{"text": f"r{i}", "value": i} for i in range(1000)]
    p = tmp_path / "f.jsonl"
    _write_fixture(p, records)
    for parts in (1, 2, 4, 8):
        plan = plan_file(p, parts=parts)
        completed = subprocess.run(
            [str(CLI), "partition", str(p), "--parts", str(parts)],
            capture_output=True,
            check=True,
            timeout=60,
        )
        rows = [
            tuple(int(f) for f in line.split("\t"))
            for line in completed.stdout.decode("ascii").splitlines()
        ]
        cli_ranges = [(start, end, length) for _i, start, end, length in rows]
        api_ranges = [(r.start, r.end, r.length) for r in plan.ranges]
        assert cli_ranges == api_ranges
