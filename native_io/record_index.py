"""Load and verify sparse record-index sidecars.

An index records every ``stride``-th record start of one immutable file
together with the file identity and framing. Plans can then be derived
without rescanning the source. This module mirrors the Rust planner
exactly so Python workers derive the same ranges as the CLI.

Usage::

    from native_io.record_index import (
        load_verified_index,
        plan_partition_boundaries_from_index,
    )

    index = load_verified_index("huge.jsonl.mmapidx")
    ranges = plan_partition_boundaries_from_index(index, 32, index.source_size)
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from native_io.plan_manifest import (
    PlanFormatError,
    _parse_framing,
    _require,
    verify_identity,
)

INDEX_SCHEMA = "mmap-chunker-index"
INDEX_SCHEMA_VERSION = 1


@dataclass(frozen=True)
class RecordIndex:
    """Parsed sparse record index."""

    schema_version: int
    generator_name: str
    generator_version: str
    source_path: str
    source_size: int
    identity: dict[str, Any]
    framing: dict[str, Any]
    stride: int
    record_count: int
    offsets: list[int]

    @property
    def framing_strategy(self) -> str:
        return str(self.framing.get("strategy", ""))


def parse_index(document: str | bytes | dict[str, Any]) -> RecordIndex:
    """Parse and validate an index document."""
    if isinstance(document, dict):
        root = document
    else:
        try:
            root = json.loads(document)
        except (ValueError, UnicodeDecodeError) as exc:
            raise PlanFormatError("index is not valid JSON: %s" % exc) from exc
    if not isinstance(root, dict):
        raise PlanFormatError("index root must be a JSON object")

    schema = _require(root, "schema", str)
    if schema != INDEX_SCHEMA:
        raise PlanFormatError("unexpected index schema %r" % schema)
    schema_version = _require(root, "schema_version", int)
    if schema_version != INDEX_SCHEMA_VERSION:
        raise PlanFormatError(
            "unsupported index schema_version %r (expected %d)"
            % (schema_version, INDEX_SCHEMA_VERSION)
        )

    generator = _require(root, "generator", dict)
    source = _require(root, "source", dict)
    framing = _parse_framing(_require(root, "framing", dict))
    index_block = _require(root, "index", dict)

    stride = _require(index_block, "stride", int)
    if stride <= 0:
        raise PlanFormatError("stride must be > 0")
    record_count = _require(index_block, "record_count", int)
    if record_count < 0:
        raise PlanFormatError("record_count must be >= 0")

    raw_offsets = _require(index_block, "record_offsets", list)
    offsets: list[int] = []
    for position, value in enumerate(raw_offsets):
        if not isinstance(value, int) or isinstance(value, bool):
            raise PlanFormatError("offset %d must be an integer" % position)
        offsets.append(value)

    source_size = _require(source, "size", int)
    if offsets:
        if offsets[0] != 0:
            raise PlanFormatError("offsets must start at 0")
        if any(
            previous >= following for previous, following in zip(offsets, offsets[1:])
        ):
            raise PlanFormatError("offsets must be strictly increasing")
        if offsets[-1] >= source_size:
            raise PlanFormatError("offset is outside the source")
    expected_slots = (record_count + stride - 1) // stride if record_count > 0 else 0
    if len(offsets) != expected_slots:
        raise PlanFormatError(
            "expected %d recorded offsets for record_count=%d stride=%d, got %d"
            % (expected_slots, record_count, stride, len(offsets))
        )

    identity = _require(source, "identity", dict)
    identity = dict(identity)
    identity.setdefault("size", source_size)

    return RecordIndex(
        schema_version=schema_version,
        generator_name=_require(generator, "name", str),
        generator_version=_require(generator, "version", str),
        source_path=_require(source, "path", str),
        source_size=source_size,
        identity=identity,
        framing=framing,
        stride=stride,
        record_count=record_count,
        offsets=offsets,
    )


def load_index(index_path: str | Path) -> RecordIndex:
    """Load and structurally validate an index file."""
    path = Path(index_path)
    try:
        document = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise PlanFormatError("cannot read index %s: %s" % (path, exc)) from exc
    return parse_index(document)


def verify_index(
    index: RecordIndex,
    file_path: str | Path | None = None,
    *,
    check_metadata: bool = True,
) -> None:
    """Verify an index against the current file identity."""
    verify_identity(
        index.identity,
        file_path,
        fallback_path=index.source_path,
        expected_size=index.source_size,
        check_metadata=check_metadata,
    )


def load_verified_index(
    index_path: str | Path,
    file_path: str | Path | None = None,
    *,
    check_metadata: bool = True,
) -> RecordIndex:
    """Load an index and verify it against the source in one call."""
    index = load_index(index_path)
    verify_index(index, file_path, check_metadata=check_metadata)
    return index


def plan_partition_boundaries_from_index(
    index: RecordIndex,
    num_partitions: int,
    file_size: int,
) -> list[tuple[int, int]]:
    """Derive record-aligned ranges exactly like the Rust planner.

    Uses record-count balancing: cut ``i`` of ``N`` maps to record
    ``floor(record_count * i / N)`` rounded to the nearest recorded slot.
    """
    if file_size != index.source_size:
        raise PlanFormatError(
            "file size %d does not match indexed size %d"
            % (file_size, index.source_size)
        )
    if file_size == 0 or num_partitions == 0 or index.record_count == 0:
        return []
    if num_partitions == 1:
        return [(0, file_size)]
    if not index.offsets:
        raise PlanFormatError("non-empty index must contain offsets")

    n = min(num_partitions, file_size)
    boundaries: list[int] = []
    last_boundary = 0
    for partition in range(1, n):
        target_record = index.record_count * partition // n
        slot = min(
            (target_record + index.stride // 2) // index.stride,
            len(index.offsets) - 1,
        )
        boundary = min(index.offsets[slot], file_size)
        if boundary == 0 or boundary <= last_boundary or boundary >= file_size:
            continue
        boundaries.append(boundary)
        last_boundary = boundary

    ranges: list[tuple[int, int]] = []
    previous = 0
    for boundary in boundaries:
        if boundary > previous:
            ranges.append((previous, boundary))
        previous = boundary
    if previous < file_size:
        ranges.append((previous, file_size))
    return ranges
