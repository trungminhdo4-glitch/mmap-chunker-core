#!/usr/bin/env python3
"""fsspec transport variant of the range reader.

For the adoption benchmark only. It reads the exact same mmap-chunker manifest
ranges as :class:`RangeJsonlReader`, but acquires the range bytes through
fsspec's local filesystem open/seek/read instead of a plain builtin seek/read,
isolating fsspec transport cost.

``fsspec.utils.read_block`` is deliberately NOT used here: its delimiter
alignment is forward-only (``seek_delimiter`` seeks to the first delimiter at
or after the requested offset), so it skips the first record of any mid-file
range and cannot reproduce an arbitrary record-aligned ``[start, end)`` range.
That boundary behaviour is demonstrated separately in
``datatrove_single_file_proof.py`` (``fsspec_boundary_demonstration``).
"""

from __future__ import annotations

import fsspec

from datatrove_jsonl_range_reader import RangeAssignment, RangeJsonlReader


class FsBlockRangeReader(RangeJsonlReader):
    name = "🦀 Range Jsonl (fsspec open transport)"

    def _read_range(self, assignment: RangeAssignment):
        fs = fsspec.filesystem("file")
        with fs.open(self.plan.file_path, "rb") as fh:
            fh.seek(assignment.start)
            data = fh.read(assignment.length)
        if len(data) != assignment.length:
            raise OSError(
                f"fsspec read returned {len(data)} bytes, expected "
                f"{assignment.length} for range "
                f"[{assignment.start}, {assignment.end_exclusive})"
            )
        yield from self._documents_from_bytes(data, assignment)
