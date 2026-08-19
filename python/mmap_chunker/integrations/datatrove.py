"""DataTrove integration for mmap-chunker-core.

Adapts the proven RangeJsonlReader (PR #24) to the installed Python API:

    from mmap_chunker import plan_file
    from mmap_chunker.integrations.datatrove import RangeJsonlReader

    plan = plan_file(path, parts=4)
    reader = RangeJsonlReader(path, plan)

The reader reproduces DataTrove's :class:`~datatrove.pipeline.readers.jsonl.JsonlReader`
document semantics: the default adapter (``text``/``id``/``media``/``metadata``),
``file_path`` metadata, per-line ``orjson`` parsing with warning-and-skip on
malformed JSON, base64 ``media_bytes`` handling, and the default
``id = f"{filepath}/{line_index}"`` scheme using the *global* line index.

Supported contract (unchanged from the adoption proof):

* local, regular, immutable files only (no remote / object-store)
* uncompressed JSONL/NDJSON, newline-delimited records
* UTF-8 content
* byte ranges are ``[start, end_exclusive)``
* each source record belongs to exactly one rank; no record crosses a rank
  boundary
* missing final newline and empty files work

Unsupported inputs (compressed files, remote paths, CSV semantics,
non-UTF-8 payloads) are rejected or explicitly declined.

This module imports ``datatrove`` and ``orjson`` lazily; the base
mmap-chunker-core package works without them.
"""

from __future__ import annotations

import base64
import mmap
import os
from pathlib import Path
from typing import Iterable

from mmap_chunker.planning import Plan

try:
    from datatrove.pipeline.readers.base import BaseDiskReader
except ImportError:
    raise ImportError(
        "mmap_chunker.integrations.datatrove requires the `datatrove` and "
        "`orjson` packages. Install them with "
        "`pip install mmap-chunker-core[datatrove]`."
    ) from None

_DEFAULT_DELIMITER_BYTE = 0x0A

_SUPPORTED_SUFFIXES = {".jsonl", ".ndjson", ".jsonlines"}
_UNSUPPORTED_SUFFIXES = {".gz", ".zst", ".gzip", ".bz2", ".xz", ".zip"}


def _require_datatrove() -> None:
    try:
        import datatrove  # noqa: F401
        import orjson  # noqa: F401
    except ImportError as exc:
        raise ImportError(
            "mmap_chunker.integrations.datatrove requires the `datatrove` and "
            "`orjson` packages. Install them with "
            "`pip install mmap-chunker-core[datatrove]`."
        ) from exc


def _record_offsets(plan: Plan) -> list[int]:
    """Global line index of each range's first record.

    Partitions are contiguous and each non-final range ends immediately after
    a newline (guaranteed by :func:`mmap_chunker.plan_file`), so the number of
    complete records before a range equals the newline count of the preceding
    ranges, plus one for a final range that does not end in a newline. This
    single mmap pass is part of manifest construction and is separate from
    native planning time.
    """
    if not plan.ranges:
        return []
    offsets: list[int] = []
    running = 0
    with open(plan.path, "rb") as fh:
        with mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ) as mapping:
            for i, r in enumerate(plan.ranges):
                offsets.append(running)
                segment = mapping[r.start : r.end]
                newlines = segment.count(b"\n")
                is_final = i == len(plan.ranges) - 1
                if is_final and segment and segment[-1:] != b"\n":
                    running += newlines + 1
                else:
                    running += newlines
    return offsets


def _split_lines(text: str) -> list[str]:
    """Split decoded text into lines matching DataTrove's text-mode reading.

    Text-mode iteration treats ``\\n`` (and, via universal newlines, ``\\r\\n``)
    as line separators and never yields a phantom empty line for a trailing
    newline. A single trailing ``\\r`` is stripped to reproduce universal-newline
    behaviour for CRLF files.
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [line[:-1] if line.endswith("\r") else line for line in lines]


class RangeJsonlReader(BaseDiskReader):
    """Reads only the ranks' planned byte ranges, reproducing JsonlReader.

    The executor passes ``(rank, world_size)`` to :meth:`run`; each rank looks
    up its pre-planned range in the :class:`~mmap_chunker.planning.Plan` and
    parses only those bytes, reproducing JsonlReader document semantics with
    global line-index IDs.
    """

    name = "🦀 Range Jsonl"
    _requires_dependencies = ["orjson"]

    def __init__(self, path, plan: Plan, **kwargs):
        """Create a range reader for one planned file.

        Args:
            path: The source file (str or os.PathLike). Must match the file
                that ``plan`` was built for.
            plan: A :class:`~mmap_chunker.planning.Plan` produced by
                :func:`mmap_chunker.plan_file` for ``path``.
            **kwargs: Forwarded to DataTrove's BaseDiskReader (limit, skip,
                adapter, text_key, id_key, default_metadata, add_file_path,
                shuffle_files, ...).
        """
        _require_datatrove()

        source_path = Path(path).resolve()
        if str(Path(plan.path).resolve()) != str(source_path):
            raise ValueError(f"plan was built for {plan.path!r}, not {source_path}")
        self._source_path = str(source_path)
        self._source_name = source_path.name
        self._plan = plan
        self._offsets = _record_offsets(plan)
        # The data folder is derived from the source path; the caller only
        # passes the file plus the plan.
        super().__init__(str(source_path.parent), **kwargs)
        self.source_file = self._source_name

    def run(self, data=None, rank: int = 0, world_size: int = 1) -> Iterable:
        """Yield this rank's documents in source order."""
        if data:
            yield from data
        if not 0 <= rank < world_size:
            raise ValueError(f"rank must be in [0, world_size), got {rank}")
        assignment = self._plan.range_for_part(rank)
        if assignment is None or assignment.length <= 0:
            return
        self.stat_update("input_files")
        ndocs = 0
        for document in self._read_range(assignment):
            if self.skip and ndocs < self.skip:
                continue
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

    def _read_range(self, assignment) -> Iterable:
        with open(self._plan.path, "rb") as fh:
            fh.seek(assignment.start)
            data = fh.read(assignment.length)
        if len(data) != assignment.length:
            raise OSError(
                f"short read: expected {assignment.length} bytes, got {len(data)}"
            )
        yield from self._documents_from_bytes(data, assignment)

    def _documents_from_bytes(self, data: bytes, assignment) -> Iterable:
        from datatrove.utils.logging import logger
        import orjson
        from orjson import JSONDecodeError

        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as error:
            logger.warning(
                f"File `{self._plan.path}` may be corrupted: "
                f"raised UnicodeDecodeError ({error})"
            )
            return
        for local_li, line in enumerate(_split_lines(text)):
            try:
                parsed = orjson.loads(line)
                for media in parsed.get("media", []):
                    if media["media_bytes"] is not None:
                        media["media_bytes"] = base64.decodebytes(
                            media["media_bytes"].encode("ascii")
                        )
                global_li = self._offsets[assignment.index] + local_li
                document = self.get_document_from_dict(
                    parsed, self._source_name, global_li
                )
                if not document:
                    continue
            except (EOFError, JSONDecodeError) as error:
                logger.warning(f"Error when reading `{self._plan.path}`: {error}")
                continue
            yield document


def build_range_reader_pipeline(path, plan: Plan, **kwargs) -> list:
    """DataTrove pipeline (a single range reader) for LocalPipelineExecutor."""
    return [RangeJsonlReader(path, plan, **kwargs)]
