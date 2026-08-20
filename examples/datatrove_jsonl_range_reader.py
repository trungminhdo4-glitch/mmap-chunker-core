#!/usr/bin/env python3
"""Range-backed JSONL reader bridging mmap-chunker-core into DataTrove.

This module proves that one immutable local JSONL file can become useful
parallel DataTrove work. A controller runs the existing ``mmap-chunker
partition`` CLI exactly once, obtains a deterministic manifest of
record-aligned byte ranges, and distributes those immutable ranges to
DataTrove ranks through :class:`RangeJsonlReader`.

The reader only supports the contract mmap-chunker-core can safely
guarantee:

* local, regular, immutable files only (no remote / object-store)
* uncompressed JSONL/NDJSON, newline-delimited records
* UTF-8 content
* byte ranges are ``[start, end_exclusive)``
* each source record belongs to exactly one rank; no record crosses a
  task ownership boundary
* missing final newline and empty files work

Unsupported inputs (compressed files, remote paths, CSV semantics,
non-UTF-8 payloads) are rejected or explicitly declined rather than
silently falling back.

The reader reproduces DataTrove's :class:`~datatrove.pipeline.readers.jsonl.JsonlReader`
document semantics exactly: the default adapter (``text``/``id``/``media``/
``metadata``), ``file_path`` metadata, per-line ``orjson`` parsing with
warning-and-skip on malformed JSON, base64 ``media_bytes`` handling, and the
default ``id = f"{filepath}/{line_index}"`` scheme using the *global* line
index so IDs are byte-for-byte identical to a single-task reader. ``skip`` and
``limit`` retain DataTrove's per-task semantics; use one requested part for a
whole-file global skip or limit.

Requires the ``datatrove`` package and ``orjson`` (normally installed via
``datatrove[io]``). The planner additionally requires the standalone
``mmap-chunker`` CLI (``cargo build --release`` produces it).
"""

from __future__ import annotations

import base64
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import subprocess
import time
from typing import Iterable

import orjson
from orjson import JSONDecodeError

from datatrove.pipeline.readers.base import BaseDiskReader
from datatrove.utils.logging import logger

DEFAULT_DELIMITER = 0x0A
PLANNER_TIMEOUT = 120.0
OFFSET_SCAN_BLOCK_SIZE = 1024 * 1024


@dataclass(frozen=True)
class RangeAssignment:
    """One immutable byte range owned by one rank.

    ``start``/``end_exclusive`` are the byte range of this rank's partition
    (``end_exclusive`` is exclusive, in line with the CLI contract).
    ``record_offset`` is the number of complete records that precede this
    range in the file, used to reproduce DataTrove's global line-index IDs.
    """

    rank: int
    start: int
    end_exclusive: int
    length: int
    record_offset: int


@dataclass(frozen=True)
class SingleFilePlan:
    """Deterministic, pickle-safe plan for a single immutable local file."""

    file_path: str
    file_size: int
    requested_parts: int
    assignments: tuple[RangeAssignment, ...]
    delimiter: int
    planner_cmd: tuple[str, ...]
    planner_wall_s: float
    partition_stdout_sha256: str

    def assignment_for_rank(self, rank: int) -> RangeAssignment | None:
        if rank < 0 or rank >= len(self.assignments):
            return None
        return self.assignments[rank]


def parse_partition_tsv(stdout: bytes) -> list[tuple[int, int, int, int]]:
    """Parse the four-column ``index<TAB>start<TAB>end<TAB>length`` TSV."""
    try:
        text = stdout.decode("ascii")
    except UnicodeDecodeError as error:
        raise AssertionError("planner TSV was not ASCII") from error
    rows: list[tuple[int, int, int, int]] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        fields = line.split("\t")
        if len(fields) != 4:
            raise AssertionError(
                f"planner line {line_number} has {len(fields)} fields, expected four"
            )
        try:
            values = [int(field) for field in fields]
        except ValueError as error:
            raise AssertionError(
                f"planner line {line_number} contains a non-numeric field"
            ) from error
        index, start, end, length = values
        if length != end - start:
            raise AssertionError(
                f"planner line {line_number} length does not match range"
            )
        rows.append((index, start, end, length))
    return rows


def invoke_partition(
    cli: Path,
    path: Path,
    parts: int,
    delimiter: int = DEFAULT_DELIMITER,
    timeout: float = PLANNER_TIMEOUT,
) -> bytes:
    """Run ``mmap-chunker partition PATH --parts N`` exactly once."""
    if parts < 1:
        raise ValueError("--parts must be positive")
    arguments = [str(cli), "partition", str(path), "--parts", str(parts)]
    if delimiter != DEFAULT_DELIMITER:
        arguments.extend(["--delimiter-byte", str(delimiter)])
    try:
        completed = subprocess.run(
            arguments,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        raise TimeoutError(
            f"planner phase timed out after {timeout:.1f}s: {cli}"
        ) from error
    except OSError as error:
        raise RuntimeError(f"planner phase could not execute {cli}: {error}") from error
    if completed.returncode != 0:
        stderr = completed.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(
            f"planner phase failed: status={completed.returncode} stderr={stderr}"
        )
    if completed.stderr:
        raise RuntimeError(
            f"planner phase wrote unexpected stderr: {completed.stderr!r}"
        )
    return completed.stdout


def _compute_record_offsets(
    path: Path, rows: list[tuple[int, int, int, int]]
) -> list[int]:
    """Global line index of each range's first record.

    Partitions are contiguous and each non-final range ends immediately
    after a newline, so the number of complete lines in a range equals its
    newline count, plus one for a final range that does not end in a newline.
    This bounded scan is part of manifest construction and is reported
    separately from Rust planning wall time.
    """
    if not rows:
        return []
    offsets: list[int] = []
    running = 0
    with open(path, "rb") as fh:
        for i, (_index, start, end, length) in enumerate(rows):
            offsets.append(running)
            fh.seek(start)
            remaining = length
            newlines = 0
            last_byte = b""
            while remaining:
                block = fh.read(min(remaining, OFFSET_SCAN_BLOCK_SIZE))
                if not block:
                    raise OSError(
                        f"short read while counting record offsets for range "
                        f"[{start}, {end})"
                    )
                newlines += block.count(b"\n")
                last_byte = block[-1:]
                remaining -= len(block)
            is_final = i == len(rows) - 1
            if is_final and length and last_byte != b"\n":
                running += newlines + 1
            else:
                running += newlines
    return offsets


def plan_single_file(
    cli: Path,
    path: Path,
    parts: int,
    delimiter: int = DEFAULT_DELIMITER,
    timeout: float = PLANNER_TIMEOUT,
) -> SingleFilePlan:
    """Plan one immutable local file once and build a pickle-safe manifest."""
    path = path.resolve()
    file_size = os.path.getsize(path)
    if not os.path.isfile(path):
        raise RuntimeError(f"not a regular local file: {path}")
    started = time.perf_counter()
    stdout = invoke_partition(cli, path, parts, delimiter, timeout)
    planner_wall_s = time.perf_counter() - started
    rows = parse_partition_tsv(stdout)
    offsets = _compute_record_offsets(path, rows)
    assignments = tuple(
        RangeAssignment(
            rank=i,
            start=start,
            end_exclusive=end,
            length=length,
            record_offset=offsets[i],
        )
        for i, (index, start, end, length) in enumerate(rows)
    )
    return SingleFilePlan(
        file_path=str(path),
        file_size=file_size,
        requested_parts=parts,
        assignments=assignments,
        delimiter=delimiter,
        planner_cmd=tuple(cli.parts),
        planner_wall_s=planner_wall_s,
        partition_stdout_sha256=hashlib.sha256(stdout).hexdigest(),
    )


def _split_lines(text: str) -> list[str]:
    """Split decoded text into lines matching DataTrove's text-mode reading.

    Text-mode iteration in DataTrove treats ``\\n`` (and, via universal
    newlines, ``\\r\\n``) as line separators and never yields a phantom
    empty line for a trailing newline. A single trailing ``\\r`` is stripped
    to reproduce universal-newline behaviour for CRLF files.
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [line[:-1] if line.endswith("\r") else line for line in lines]


class RangeJsonlReader(BaseDiskReader):
    """DataTrove reader that reads only its rank's immutable byte range.

    The executor passes ``(rank, world_size)`` to ``run()``; each rank looks
    up its pre-planned range in the manifest and parses only those bytes,
    reproducing :class:`JsonlReader` document semantics with global line-index
    IDs.
    """

    name = "🦀 Range Jsonl"
    _requires_dependencies = ["orjson"]

    def __init__(
        self,
        data_folder,
        source_file: str,
        plan: SingleFilePlan,
        paths_file=None,
        limit: int = -1,
        skip: int = 0,
        file_progress: bool = False,
        doc_progress: bool = False,
        adapter=None,
        text_key: str = "text",
        id_key: str = "id",
        default_metadata: dict | None = None,
        recursive: bool = False,
        glob_pattern: str | None = None,
        shuffle_files: bool = False,
        add_file_path: bool = True,
    ):
        super().__init__(
            data_folder,
            paths_file,
            limit,
            skip,
            file_progress,
            doc_progress,
            adapter,
            text_key,
            id_key,
            default_metadata,
            recursive,
            glob_pattern,
            shuffle_files,
            add_file_path,
        )
        self.source_file = source_file
        self.plan = plan
        if (
            plan.file_path != str(Path(data_folder).resolve() / source_file)
            and not Path(plan.file_path).name == source_file
        ):
            raise ValueError(
                f"plan file {plan.file_path!r} does not match data_folder/source_file "
                f"{data_folder!r}/{source_file!r}"
            )

    def run(self, data=None, rank: int = 0, world_size: int = 1):
        if world_size != self.plan.requested_parts:
            raise ValueError(
                "world_size must equal plan.requested_parts for deterministic "
                f"range ownership, got world_size={world_size}, "
                f"plan.requested_parts={self.plan.requested_parts}"
            )
        if not 0 <= rank < world_size:
            raise ValueError(f"rank must be in [0, world_size), got {rank}")
        if data:
            yield from data
        assignment = self.plan.assignment_for_rank(rank)
        if assignment is None or assignment.length <= 0:
            if not self.plan.assignments and rank == 0:
                self.stat_update("input_files")
                self.stat_update("documents", value=0, unit="input_file")
            return
        # The N ranges are views of one logical input file. Count that source
        # once in aggregate stats, like BaseDiskReader does for one file.
        if rank == 0:
            self.stat_update("input_files")
        skipped = 0
        ndocs = 0
        for document in self._read_range(assignment):
            if skipped < self.skip:
                skipped += 1
                continue
            if self.limit != -1 and ndocs >= self.limit:
                break
            ndocs += 1
            self.update_doc_stats(document)
            yield document
        self.stat_update("documents", value=ndocs, unit="input_file")

    def read_file(self, filepath: str):
        """Not used: this reader is range-based and overrides ``run()``."""
        raise NotImplementedError(
            "RangeJsonlReader is range-based; it overrides run() and does not "
            "read whole files through read_file()."
        )

    def _read_range(self, assignment: RangeAssignment) -> Iterable:
        with open(self.plan.file_path, "rb") as fh:
            fh.seek(assignment.start)
            offset = assignment.start
            local_li = 0
            while offset < assignment.end_exclusive:
                line = fh.readline()
                if not line:
                    raise OSError(
                        f"short read: expected range [{assignment.start}, "
                        f"{assignment.end_exclusive}), stopped at {offset}"
                    )
                offset += len(line)
                if offset > assignment.end_exclusive:
                    raise OSError(
                        f"range [{assignment.start}, {assignment.end_exclusive}) "
                        "splits a record"
                    )
                yield from self._documents_from_line(line, assignment, local_li)
                local_li += 1

    def _documents_from_line(
        self, line: bytes, assignment: RangeAssignment, local_li: int
    ) -> Iterable:
        """Parse one bounded line into DataTrove documents."""
        try:
            with self.track_time():
                text = line.decode("utf-8")
                parsed = orjson.loads(text)
                for media in parsed.get("media", []):
                    if media["media_bytes"] is not None:
                        media["media_bytes"] = base64.decodebytes(
                            media["media_bytes"].encode("ascii")
                        )
                global_li = assignment.record_offset + local_li
                document = self.get_document_from_dict(
                    parsed, self.source_file, global_li
                )
                if document:
                    yield document
        except UnicodeDecodeError as error:
            logger.warning(
                f"File `{self.plan.file_path}` may be corrupted: "
                f"raised UnicodeDecodeError ({error})"
            )
            return
        except (EOFError, JSONDecodeError) as error:
            logger.warning(f"Error when reading `{self.plan.file_path}`: {error}")
            return


def build_range_reader_pipeline(
    data_folder, source_file: str, plan: SingleFilePlan, **kwargs
) -> list:
    """DataTrove pipeline (a single range reader) for ``LocalPipelineExecutor``."""
    return [RangeJsonlReader(data_folder, source_file, plan, **kwargs)]
