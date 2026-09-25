"""Correctness matrix for mmap_chunker.plan_file_ranges (v1.5 sources).

Runs against the installed package (or the repository source tree).
Requires the bundled native library (wheel build or release-tree probe).
Each case verifies backend parity with plan_file, exact coverage, record
alignment, determinism, and input validation before any native call.
"""

from __future__ import annotations

import json
import random
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

from mmap_chunker import PlanningError, plan_file, plan_file_ranges  # noqa: E402
from mmap_chunker import planning  # noqa: E402

SOURCES = ["mmap", "windowed", "pread"]


def _write_records(path: Path, texts: list[str]) -> None:
    with open(path, "wb") as fh:
        for text in texts:
            fh.write(json.dumps({"text": text}).encode("utf-8"))
            fh.write(b"\n")


def _dense_texts(n: int, seed: int = 7) -> list[str]:
    rng = random.Random(seed)
    return [
        "".join(rng.choice("abcdefghij") for _ in range(rng.randint(20, 120)))
        for _ in range(n)
    ]


def test_sources_match_plan_file(tmp_path: Path) -> None:
    """All backends emit byte-identical plans to plan_file."""
    p = tmp_path / "f.jsonl"
    _write_records(p, _dense_texts(500))
    expected = plan_file(p, parts=8)
    for source in SOURCES:
        got = plan_file_ranges(p, parts=8, source=source)
        assert got.ranges == expected.ranges, source
        assert (got.path, got.file_size, got.requested_parts) == (
            expected.path,
            expected.file_size,
            expected.requested_parts,
        )


@pytest.mark.parametrize("parts", [1, 2, 3, 7, 16, 64])
def test_parts_matrix_all_sources(tmp_path: Path, parts: int) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, _dense_texts(300))
    expected = [(r.start, r.end) for r in plan_file(p, parts=parts).ranges]
    for source in SOURCES:
        got = [
            (r.start, r.end)
            for r in plan_file_ranges(p, parts=parts, source=source).ranges
        ]
        assert got == expected, (source, parts)


def test_sparse_and_giant_records(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    texts = ["x" * (1024 * 1024), "tiny", "y" * (2 * 1024 * 1024 + 13), "z"]
    _write_records(p, texts)
    expected = [(r.start, r.end) for r in plan_file(p, parts=4).ranges]
    for source in SOURCES:
        got = [
            (r.start, r.end) for r in plan_file_ranges(p, parts=4, source=source).ranges
        ]
        assert got == expected, source


def test_missing_final_newline_and_empty(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, _dense_texts(50))
    data = p.read_bytes()
    p.write_bytes(data[:-1])
    expected = [(r.start, r.end) for r in plan_file(p, parts=4).ranges]
    for source in SOURCES:
        got = [
            (r.start, r.end) for r in plan_file_ranges(p, parts=4, source=source).ranges
        ]
        assert got == expected, source
    q = tmp_path / "empty.jsonl"
    q.write_bytes(b"")
    for source in SOURCES:
        assert plan_file_ranges(q, parts=4, source=source).ranges == ()


def test_custom_delimiter_byte(tmp_path: Path) -> None:
    p = tmp_path / "f.csv"
    p.write_bytes(b"a,b,c,d,e,f,g,h\n".replace(b"\n", b",")[:-1])
    expected = [
        (r.start, r.end) for r in plan_file(p, parts=3, delimiter=ord(",")).ranges
    ]
    for source in SOURCES:
        got = [
            (r.start, r.end)
            for r in plan_file_ranges(
                p, parts=3, delimiter=ord(","), source=source
            ).ranges
        ]
        assert got == expected, source


def test_small_window_parity(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, _dense_texts(500))
    expected = [(r.start, r.end) for r in plan_file(p, parts=8).ranges]
    got = [
        (r.start, r.end)
        for r in plan_file_ranges(
            p, parts=8, source="windowed", window_bytes=65536
        ).ranges
    ]
    assert got == expected


def test_deterministic_repeated_plan(tmp_path: Path) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, _dense_texts(200))
    first = plan_file_ranges(p, parts=5, source="pread")
    for _ in range(4):
        assert plan_file_ranges(p, parts=5, source="pread").ranges == first.ranges


def test_reject_unknown_source_before_native_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, ["x"])

    def unexpected_native_load():
        pytest.fail("native library must not be loaded for an invalid source")

    monkeypatch.setattr(planning._native, "get_library", unexpected_native_load)

    with pytest.raises(ValueError, match="one of"):
        plan_file_ranges(p, parts=4, source="auto")
    with pytest.raises(TypeError, match="one of"):
        plan_file_ranges(p, parts=4, source=None)  # type: ignore[arg-type]


def test_reject_window_bytes_before_native_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, ["x"])

    def unexpected_native_load():
        pytest.fail("native library must not be loaded for invalid window_bytes")

    monkeypatch.setattr(planning._native, "get_library", unexpected_native_load)

    with pytest.raises(ValueError, match=">= 65536"):
        plan_file_ranges(p, parts=4, source="windowed", window_bytes=65535)
    with pytest.raises(ValueError, match="requires source='windowed'"):
        plan_file_ranges(p, parts=4, source="mmap", window_bytes=65536)
    with pytest.raises(ValueError, match="requires source='windowed'"):
        plan_file_ranges(p, parts=4, source="pread", window_bytes=131072)
    with pytest.raises(TypeError, match="window_bytes"):
        plan_file_ranges(p, parts=4, source="windowed", window_bytes="big")  # type: ignore[arg-type]


def test_reject_invalid_inputs_before_native_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    p = tmp_path / "f.jsonl"
    _write_records(p, ["x"])

    def unexpected_native_load():
        pytest.fail("native library must not be loaded for invalid inputs")

    monkeypatch.setattr(planning._native, "get_library", unexpected_native_load)

    with pytest.raises(ValueError, match="parts must be >= 1"):
        plan_file_ranges(p, parts=0)
    with pytest.raises(TypeError):
        plan_file_ranges(12345, parts=4)  # type: ignore[arg-type]
    with pytest.raises(FileNotFoundError):
        plan_file_ranges(tmp_path / "missing.jsonl", parts=4)


def test_exported_from_package() -> None:
    assert "plan_file_ranges" in mmap_chunker.__all__
    assert callable(mmap_chunker.plan_file_ranges)


def test_native_helper_rejects_missing_capability(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A library without bit 7 must fail before any planning call."""
    from mmap_chunker import _native

    p = tmp_path / "f.jsonl"
    _write_records(p, ["x"])

    class FakeLib:
        def mmap_engine_capabilities(self):
            return 0x10  # only RECORD_PARTITIONING

    with pytest.raises(Exception, match="WINDOWED_PLANNING"):
        fake = FakeLib()
        _native.plan_partition_ranges(fake, str(p), 2, b"\n", 0, 65536)  # type: ignore[arg-type]
