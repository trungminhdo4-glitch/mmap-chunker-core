"""Load and verify ``mmap-chunker plan`` manifests.

A plan manifest is a reproducible worker contract: source identity,
framing, partitioning parameters, and record-aligned byte ranges. This
module is the reference consumer for Python workers and pipelines.

Usage::

    from native_io.plan_manifest import load_plan, verify_plan

    plan = load_plan("huge.jsonl.plan.json")
    verify_plan(plan)                       # against plan.source_path
    for byte_range in plan.ranges:
        ...  # process plan.source_path[byte_range.start:byte_range.end]

Verification recomputes the sampled content fingerprint mirrored from
the Rust planner, so a stale or partially rewritten source is rejected
before any worker consumes the plan.

No dependencies beyond the Python standard library.
"""

from __future__ import annotations

import json
import os
import struct
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

PLAN_SCHEMA = "mmap-chunker-plan"
PLAN_SCHEMA_VERSION = 1
IDENTITY_SAMPLE_BYTES = 64 * 1024

_FNV1A64_OFFSET_BASIS = 0xCBF29CE484222325
_FNV1A64_PRIME = 0x100000001B3
_FNV1A64_MASK = 0xFFFFFFFFFFFFFFFF
_FNV1A64_PREFIX = "fnv1a64:0x"


class PlanError(Exception):
    """Base class for plan loading/verification errors."""


class PlanFormatError(PlanError):
    """The manifest is malformed or uses an unsupported schema."""


class PlanIdentityMismatch(PlanError):
    """The manifest does not describe the current file content."""


@dataclass(frozen=True)
class Range:
    """One record-aligned byte range; ``start`` inclusive, ``end`` exclusive."""

    index: int
    start: int
    end: int

    @property
    def length(self) -> int:
        return self.end - self.start


@dataclass(frozen=True)
class Plan:
    """Parsed plan manifest."""

    schema_version: int
    planner_name: str
    planner_version: str
    source_path: str
    source_size: int
    identity: dict[str, Any]
    framing: dict[str, Any]
    requested_partitions: int
    actual_partitions: int
    source_mode: str | None
    window_bytes: int | None
    source_index: dict[str, Any] | None
    ranges: list[Range] = field(default_factory=list)

    @property
    def framing_strategy(self) -> str:
        """Framing strategy name (``delimiter``, ``fixed_width``, ...)."""
        return str(self.framing.get("strategy", ""))

    @property
    def delimiter(self) -> bytes | None:
        """Delimiter bytes for delimiter framing, else ``None``."""
        value = self.framing.get("delimiter_hex")
        if not isinstance(value, str):
            return None
        return bytes.fromhex(value)


def _fnv1a64(parts: list[bytes]) -> int:
    value = _FNV1A64_OFFSET_BASIS
    for part in parts:
        for byte in part:
            value ^= byte
            value = (value * _FNV1A64_PRIME) & _FNV1A64_MASK
    return value


def _sample_fingerprint(path: Path, size: int) -> str:
    """Mirror of the Rust ``manifest::sample_fingerprint`` implementation."""
    if size == 0:
        return _FNV1A64_PREFIX + format(_fnv1a64([struct.pack("<Q", 0)]), "016x")

    head_length = min(size, IDENTITY_SAMPLE_BYTES)
    tail_length = min(size, IDENTITY_SAMPLE_BYTES)
    with open(path, "rb") as handle:
        head = handle.read(head_length)
        if head_length + tail_length >= size:
            remaining = size - len(head)
            while remaining > 0:
                chunk = handle.read(remaining)
                if not chunk:
                    break
                head += chunk
                remaining -= len(chunk)
            return _FNV1A64_PREFIX + format(
                _fnv1a64([struct.pack("<Q", size), head]), "016x"
            )
        handle.seek(size - tail_length)
        tail = handle.read(tail_length)

    return _FNV1A64_PREFIX + format(
        _fnv1a64([struct.pack("<Q", size), head, tail]), "016x"
    )


def _require(mapping: dict[str, Any], key: str, expected_type: type) -> Any:
    if key not in mapping:
        raise PlanFormatError("missing field %r" % key)
    value = mapping[key]
    if not isinstance(value, expected_type):
        raise PlanFormatError(
            "field %r must be %s, got %r" % (key, expected_type.__name__, value)
        )
    return value


def _optional_int(mapping: dict[str, Any], key: str) -> int | None:
    value = mapping.get(key)
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool):
        raise PlanFormatError("field %r must be an integer or null" % key)
    return value


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    """JSON object hook that rejects duplicate keys.

    The stdlib decoder keeps the last duplicate silently, which would
    make two verifiers disagree about which value was sealed. Ambiguous
    manifests are rejected instead, mirroring the Rust ``from_json``
    contract.
    """
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise PlanFormatError("duplicate key %r" % key)
        result[key] = value
    return result


def parse_manifest(document: str | bytes | dict[str, Any]) -> Plan:
    """Parse and validate a manifest document."""
    if isinstance(document, dict):
        root = document
    else:
        try:
            root = json.loads(document, object_pairs_hook=_unique_object)
        except (ValueError, UnicodeDecodeError) as exc:
            raise PlanFormatError("manifest is not valid JSON: %s" % exc) from exc
    if not isinstance(root, dict):
        raise PlanFormatError("manifest root must be a JSON object")

    schema = _require(root, "schema", str)
    if schema != PLAN_SCHEMA:
        raise PlanFormatError("unexpected schema %r" % schema)
    schema_version = _require(root, "schema_version", int)
    if schema_version != PLAN_SCHEMA_VERSION:
        raise PlanFormatError(
            "unsupported schema_version %r (expected %d)"
            % (schema_version, PLAN_SCHEMA_VERSION)
        )

    planner = _require(root, "planner", dict)
    source = _require(root, "source", dict)
    framing_document = _require(root, "framing", dict)
    partitioning = _require(root, "partitioning", dict)

    framing = _parse_framing(framing_document)

    partition_strategy = _require(partitioning, "strategy", str)
    if partition_strategy not in ("bytes", "indexed_records"):
        raise PlanFormatError(
            "unsupported partitioning strategy %r" % partition_strategy
        )

    source_index = partitioning.get("source_index")
    if source_index is not None:
        if not isinstance(source_index, dict):
            raise PlanFormatError("source_index must be an object or null")
        _require(source_index, "stride", int)
        _require(source_index, "record_count", int)

    raw_ranges = _require(root, "ranges", list)
    ranges: list[Range] = []
    for expected_index, entry in enumerate(raw_ranges):
        if not isinstance(entry, dict):
            raise PlanFormatError("range %d must be an object" % expected_index)
        index = _require(entry, "index", int)
        start = _require(entry, "start", int)
        end = _require(entry, "end", int)
        length = _require(entry, "length", int)
        if index != expected_index:
            raise PlanFormatError(
                "range index %d is out of order (expected %d)" % (index, expected_index)
            )
        if end <= start:
            raise PlanFormatError("range %d is empty or inverted" % index)
        if length != end - start:
            raise PlanFormatError(
                "range %d length %d does not match [%d, %d)"
                % (index, length, start, end)
            )
        ranges.append(Range(index=index, start=start, end=end))

    actual_partitions = _require(partitioning, "actual_partitions", int)
    if actual_partitions != len(ranges):
        raise PlanFormatError(
            "actual_partitions %d does not match %d ranges"
            % (actual_partitions, len(ranges))
        )

    identity = _require(source, "identity", dict)
    return Plan(
        schema_version=schema_version,
        planner_name=_require(planner, "name", str),
        planner_version=_require(planner, "version", str),
        source_path=_require(source, "path", str),
        source_size=_require(source, "size", int),
        identity=dict(identity),
        framing=framing,
        requested_partitions=_require(partitioning, "requested_partitions", int),
        actual_partitions=actual_partitions,
        source_mode=_optional_str(partitioning, "source_mode"),
        window_bytes=_optional_int(partitioning, "window_bytes"),
        source_index=dict(source_index) if source_index is not None else None,
        ranges=ranges,
    )


def _parse_framing(framing: dict[str, Any]) -> dict[str, Any]:
    """Validate a framing descriptor and return a normalized copy."""
    strategy = _require(framing, "strategy", str)
    if strategy == "delimiter":
        delimiter_hex = _require(framing, "delimiter_hex", str)
        try:
            delimiter = bytes.fromhex(delimiter_hex)
        except ValueError as exc:
            raise PlanFormatError("delimiter_hex is not valid hex: %s" % exc) from exc
        if len(delimiter) == 0:
            raise PlanFormatError("delimiter_hex must not be empty")
        delimiter_len = _require(framing, "delimiter_len", int)
        if delimiter_len != len(delimiter):
            raise PlanFormatError(
                "delimiter_len %d does not match delimiter_hex length %d"
                % (delimiter_len, len(delimiter))
            )
        return {
            "strategy": "delimiter",
            "delimiter_hex": delimiter_hex,
            "delimiter_len": delimiter_len,
        }
    if strategy == "fixed_width":
        record_bytes = _require(framing, "record_bytes", int)
        if record_bytes <= 0:
            raise PlanFormatError("record_bytes must be > 0")
        return {"strategy": "fixed_width", "record_bytes": record_bytes}
    if strategy == "length_prefixed":
        prefix_bytes = _require(framing, "prefix_bytes", int)
        if not 1 <= prefix_bytes <= 8:
            raise PlanFormatError("prefix_bytes must be in 1..=8")
        little_endian = _require(framing, "little_endian", bool)
        length_includes_prefix = _require(framing, "length_includes_prefix", bool)
        return {
            "strategy": "length_prefixed",
            "prefix_bytes": prefix_bytes,
            "little_endian": little_endian,
            "length_includes_prefix": length_includes_prefix,
        }
    if strategy == "custom":
        return {"strategy": "custom", "name": _require(framing, "name", str)}
    raise PlanFormatError("unsupported framing strategy %r" % strategy)


def _optional_str(mapping: dict[str, Any], key: str) -> str | None:
    value = mapping.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        raise PlanFormatError("field %r must be a string or null" % key)
    return value


def load_plan(manifest_path: str | Path) -> Plan:
    """Load and structurally validate a manifest file."""
    path = Path(manifest_path)
    try:
        document = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise PlanFormatError("cannot read manifest %s: %s" % (path, exc)) from exc
    return parse_manifest(document)


def verify_ranges(plan: Plan, file_size: int | None = None) -> None:
    """Verify that ``plan.ranges`` exactly partition the source file."""
    expected_end = 0
    for byte_range in plan.ranges:
        if byte_range.start != expected_end:
            raise PlanFormatError(
                "range %d does not continue at %d (gap or overlap)"
                % (byte_range.index, expected_end)
            )
        expected_end = byte_range.end
    size = plan.source_size if file_size is None else file_size
    if plan.ranges:
        if plan.ranges[0].start != 0:
            raise PlanFormatError("first range does not start at 0")
        if expected_end != size:
            raise PlanFormatError(
                "ranges end at %d but source size is %d" % (expected_end, size)
            )
    elif size != 0:
        raise PlanFormatError("empty plan for a non-empty source")


def verify_identity(
    identity: dict[str, Any],
    file_path: str | Path | None = None,
    *,
    fallback_path: str = "",
    expected_size: int | None = None,
    check_metadata: bool = True,
) -> int:
    """Verify a source identity document against the current file.

    Always compares size (against ``expected_size`` or the identity's own
    ``size`` field) and the sampled content fingerprint. When
    ``check_metadata`` is true, also compares non-null mtime/device/inode
    fields captured by the planner.

    Returns the current file size.

    Raises:
        PlanIdentityMismatch: the file no longer matches the identity.
    """
    target = Path(file_path) if file_path is not None else Path(fallback_path)
    try:
        stat = target.stat()
    except OSError as exc:
        raise PlanIdentityMismatch(
            "cannot stat plan source %s: %s" % (target, exc)
        ) from exc

    size = identity.get("size") if expected_size is None else expected_size
    if size is not None and stat.st_size != size:
        raise PlanIdentityMismatch(
            "source size changed: expected=%d current=%d" % (size, stat.st_size)
        )

    expected_fingerprint = identity.get("sample_fingerprint")
    if expected_fingerprint:
        actual_fingerprint = _sample_fingerprint(target, stat.st_size)
        if actual_fingerprint != expected_fingerprint:
            raise PlanIdentityMismatch(
                "source fingerprint changed: plan=%s current=%s"
                % (expected_fingerprint, actual_fingerprint)
            )

    if check_metadata:
        expected_mtime = identity.get("modified_unix_nanos")
        if expected_mtime is not None and stat.st_mtime_ns != expected_mtime:
            raise PlanIdentityMismatch(
                "source mtime changed: plan=%d current=%d"
                % (expected_mtime, stat.st_mtime_ns)
            )
        expected_device = identity.get("device")
        if expected_device is not None:
            actual_device = getattr(stat, "st_dev", None)
            if actual_device is not None:
                # Windows: the planner records the 32-bit volume serial
                # from BY_HANDLE_FILE_INFORMATION, while CPython >= 3.12
                # reports the 64-bit FileIdInfo serial in st_dev. The low
                # 32 bits agree; compare those.
                if os.name == "nt":
                    actual_device &= 0xFFFFFFFF
                if actual_device != expected_device:
                    raise PlanIdentityMismatch(
                        "source device changed: plan=%d current=%d"
                        % (expected_device, actual_device)
                    )
        expected_inode = identity.get("inode")
        if expected_inode is not None:
            actual_inode = getattr(stat, "st_ino", None)
            if actual_inode is not None and actual_inode != expected_inode:
                raise PlanIdentityMismatch(
                    "source inode changed: plan=%d current=%d"
                    % (expected_inode, actual_inode)
                )

    return stat.st_size


def verify_plan(
    plan: Plan,
    file_path: str | Path | None = None,
    *,
    check_metadata: bool = True,
) -> None:
    """Verify a plan against the current file.

    Compares size, sampled content fingerprint, and (unless disabled)
    non-null mtime/device/inode fields, then checks that the ranges
    exactly partition the file.

    Raises:
        PlanIdentityMismatch: the file no longer matches the plan.
        PlanFormatError: the plan itself is internally inconsistent.
    """
    size = verify_identity(
        plan.identity,
        file_path,
        fallback_path=plan.source_path,
        expected_size=plan.source_size,
        check_metadata=check_metadata,
    )
    verify_ranges(plan, file_size=size)


def load_verified_plan(
    manifest_path: str | Path,
    file_path: str | Path | None = None,
    *,
    check_metadata: bool = True,
) -> Plan:
    """Load a manifest and verify it against the source in one call."""
    plan = load_plan(manifest_path)
    verify_plan(plan, file_path, check_metadata=check_metadata)
    return plan
