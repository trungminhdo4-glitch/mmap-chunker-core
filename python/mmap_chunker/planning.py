"""Record-aligned file planning through the mmap-chunker-core C ABI.

The public entry point is :func:`plan_file`, which returns an immutable
:class:`Plan` of record-aligned byte ranges for one local file. The native
handle is opened, scanned, and freed entirely inside the call; no returned
object depends on a live memory map.
"""

from __future__ import annotations

import ctypes
import os
import stat
from dataclasses import dataclass
from pathlib import Path
from typing import Union

from mmap_chunker import _native

DEFAULT_DELIMITER = 0x0A
_SIZE_T_MAX = (1 << (ctypes.sizeof(ctypes.c_size_t) * 8)) - 1

PathLike = Union[str, "os.PathLike[str]"]
_DelimiterInput = Union[bytes, int]


class PlanningError(RuntimeError):
    """Raised when a planning operation fails at the Python or native layer."""


@dataclass(frozen=True)
class Range:
    """One immutable, record-aligned byte range of a file.

    ``start`` and ``end`` are half-open bounds: the range covers
    ``[start, end)`` and has length ``end - start``.
    """

    index: int
    start: int
    end: int
    length: int

    def __post_init__(self) -> None:
        if self.index < 0:
            raise ValueError(f"range index must be >= 0, got {self.index}")
        if self.start < 0:
            raise ValueError(f"range start must be >= 0, got {self.start}")
        if self.end < self.start:
            raise ValueError(
                f"range end must be >= start, got {self.end} < {self.start}"
            )
        if self.length != self.end - self.start:
            raise ValueError(
                f"range length must equal end - start, got {self.length} != "
                f"{self.end} - {self.start}"
            )


@dataclass(frozen=True)
class Plan:
    """Deterministic, immutable plan for one file.

    ``ranges`` is a tuple of :class:`Range` sorted by ascending start offset,
    covering the file exactly with no gaps and no overlaps. Every non-final
    range ends immediately after a delimiter byte, so no record is split.
    """

    path: str
    file_size: int
    requested_parts: int
    delimiter: int
    ranges: tuple[Range, ...]

    @property
    def actual_partitions(self) -> int:
        """Number of ranges actually produced (may be less than requested)."""
        return len(self.ranges)

    def range_for_part(self, part: int) -> Range | None:
        """Return the range for a zero-based partition index, or None."""
        if 0 <= part < len(self.ranges):
            return self.ranges[part]
        return None


def _coerce_path(path: PathLike) -> str:
    raw = os.fspath(path)
    if isinstance(raw, bytes):
        raw = os.fsdecode(raw)
    if not isinstance(raw, str):
        raise TypeError(f"path must be str or os.PathLike, got {type(path).__name__}")
    if "\x00" in raw:
        raise ValueError("path contains an embedded NUL byte")
    return str(Path(raw).resolve())


def _coerce_parts(parts: int) -> int:
    if isinstance(parts, bool) or not isinstance(parts, int):
        raise TypeError(f"parts must be an int, got {type(parts).__name__}")
    parts = int.__int__(parts)
    if parts < 1:
        raise ValueError(f"parts must be >= 1, got {parts}")
    if parts > _SIZE_T_MAX:
        raise OverflowError(
            f"parts must be <= platform size_t maximum {_SIZE_T_MAX}, got {parts}"
        )
    return parts


def _coerce_delimiter(delimiter: _DelimiterInput) -> int:
    if isinstance(delimiter, bool):
        raise ValueError(
            f"delimiter int must be a byte value 0..255, got {delimiter!r}"
        )
    if isinstance(delimiter, int):
        delimiter = int.__int__(delimiter)
        if not 0 <= delimiter <= 255:
            raise ValueError(
                f"delimiter int must be a byte value 0..255, got {delimiter!r}"
            )
        return delimiter
    if isinstance(delimiter, bytes):
        if len(delimiter) != 1:
            raise ValueError(
                "partition delimiter must be exactly one byte; the current "
                "partition ABI accepts a single raw byte only, got "
                f"{len(delimiter)} bytes"
            )
        return delimiter[0]
    raise TypeError(
        "delimiter must be a single-byte value such as b'\\n' (or its int "
        f"value 10), got {type(delimiter).__name__}"
    )


_SOURCES = ("mmap", "windowed", "pread")
_SOURCE_MODES = {
    "mmap": _native.SOURCE_MODE_MMAP,
    "windowed": _native.SOURCE_MODE_WINDOWED,
    "pread": _native.SOURCE_MODE_PREAD,
}


def _coerce_source(source: str) -> int:
    if isinstance(source, bool) or not isinstance(source, str):
        raise TypeError(
            f"source must be one of {_SOURCES}, got {type(source).__name__}"
        )
    try:
        return _SOURCE_MODES[source]
    except KeyError:
        raise ValueError(f"source must be one of {_SOURCES}, got {source!r}") from None


def _coerce_window_bytes(window_bytes: int | None) -> int:
    if window_bytes is None:
        return _native.DEFAULT_WINDOW_BYTES
    if isinstance(window_bytes, bool) or not isinstance(window_bytes, int):
        raise TypeError(
            f"window_bytes must be an int >= {_native.MIN_WINDOW_BYTES} or None, "
            f"got {type(window_bytes).__name__}"
        )
    window_bytes = int.__int__(window_bytes)
    if window_bytes < _native.MIN_WINDOW_BYTES:
        raise ValueError(
            f"window_bytes must be >= {_native.MIN_WINDOW_BYTES}, got {window_bytes}"
        )
    return window_bytes


def _verify_file(path: str) -> int:
    try:
        info = os.stat(path)
    except FileNotFoundError:
        raise FileNotFoundError(f"input file does not exist: {path}") from None
    except OSError as exc:
        raise OSError(f"cannot access input file {path}: {exc}") from exc
    if stat.S_ISDIR(info.st_mode):
        raise IsADirectoryError(f"input path is a directory, not a file: {path}")
    if not stat.S_ISREG(info.st_mode):
        raise PlanningError(f"input path is not a regular file: {path}")
    return int(info.st_size)


def _verify_record_alignment(
    path: str, ranges: tuple[Range, ...], delimiter: int
) -> None:
    """Independently verify boundaries against the on-disk bytes.

    Each non-final range must end immediately after a delimiter byte and each
    non-first range must start immediately after a delimiter byte. Reads are
    one byte per boundary (plus the first/last byte of the file), never the
    whole file.
    """
    if not ranges:
        return
    with open(path, "rb") as fh:
        for i, r in enumerate(ranges):
            if r.start > 0:
                fh.seek(r.start - 1)
                if fh.read(1)[0] != delimiter:
                    raise PlanningError(
                        f"range {r.index} starts at byte {r.start}, but the "
                        f"preceding byte is not the delimiter (record split)"
                    )
            if i < len(ranges) - 1:
                fh.seek(r.end - 1)
                if fh.read(1)[0] != delimiter:
                    raise PlanningError(
                        f"range {r.index} ends at byte {r.end}, but the "
                        "preceding byte is not the delimiter (record split)"
                    )


def plan_file(
    path: PathLike, parts: int, delimiter: _DelimiterInput = DEFAULT_DELIMITER
) -> Plan:
    """Plan record-aligned byte ranges for one immutable local file.

    Args:
        path: A file path (str or os.PathLike). The file must exist, be a
            regular local file, and not be mutated while planning runs.
        parts: Number of desired partitions; must be >= 1 and no greater than
            the platform ``size_t`` maximum. The actual number of ranges may
            be smaller when records are sparse.
        delimiter: The single raw byte marking record boundaries. Defaults to
            the newline byte ``b"\\n"`` (also accepted as the int ``10``).
            Multi-byte partition delimiters are not supported by the current
            native partition ABI.

    Returns:
        An immutable :class:`Plan`. No returned object references the
        memory-mapped file; the native handle is released before returning.

    Raises:
        TypeError: Invalid ``parts`` or ``delimiter`` type.
        ValueError: Non-positive ``parts``, invalid delimiter value, or an
            embedded NUL in the path.
        OverflowError: ``parts`` exceeds the platform ``size_t`` maximum.
        FileNotFoundError: The input file does not exist.
        IsADirectoryError: The input path is a directory.
        PlanningError: The file is not a regular file, native planning fails,
            or a verified invariant is violated.
    """
    resolved = _coerce_path(path)
    parts = _coerce_parts(parts)
    delimiter_byte = _coerce_delimiter(delimiter)
    file_size = _verify_file(resolved)

    lib = _native.get_library()
    handle = lib.mmap_engine_open(os.fsencode(resolved))
    if not handle:
        raise PlanningError(
            f"native open failed for {resolved}: {_native.last_error(lib)}"
        )

    try:
        count = int(
            lib.mmap_engine_partition_records(
                handle, ctypes.c_size_t(parts), delimiter_byte
            )
        )
        if file_size == 0:
            if count != 0:
                raise PlanningError(
                    "native partition returned a nonzero count for an empty file"
                )
            return Plan(resolved, 0, parts, delimiter_byte, ())
        if count == 0:
            raise PlanningError(
                f"native partition planning failed: {_native.last_error(lib)}"
            )

        lengths: list[int] = []
        for index in range(count):
            view = _native._CChunkView()
            if (
                lib.mmap_engine_get_chunk(
                    handle, ctypes.c_size_t(index), ctypes.byref(view)
                )
                != 0
            ):
                raise PlanningError(
                    f"native get_chunk({index}) failed: {_native.last_error(lib)}"
                )
            lengths.append(int(view.len))
    finally:
        lib.mmap_engine_free(handle)

    # Reconstruct offsets cumulatively from lengths, then verify the result
    # independently against the file's own metadata and bytes.
    ranges: list[Range] = []
    offset = 0
    for index, length in enumerate(lengths):
        ranges.append(Range(index, offset, offset + length, length))
        offset += length

    if offset != file_size:
        raise PlanningError(
            f"partition coverage {offset} does not match file size {file_size}; "
            "the native library produced an inconsistent plan"
        )
    _verify_record_alignment(resolved, tuple(ranges), delimiter_byte)

    return Plan(resolved, file_size, parts, delimiter_byte, tuple(ranges))


def plan_file_ranges(
    path: PathLike,
    parts: int,
    delimiter: _DelimiterInput = DEFAULT_DELIMITER,
    *,
    source: str = "mmap",
    window_bytes: int | None = None,
) -> Plan:
    """Plan record-aligned byte ranges with an explicit byte source.

    Same :class:`Plan` contract as :func:`plan_file`, but the ranges are
    produced through the v1.5 source-selectable native API instead of the
    mmap-only engine path, so ``windowed`` (bounded address space) and
    ``pread`` (no mapping) backends are available without manual C-ABI
    calls. All backends emit byte-identical ranges for the same inputs.

    Args:
        path: A file path (str or os.PathLike). The file must exist, be a
            regular local file, and not be mutated while planning runs.
        parts: Number of desired partitions; must be >= 1 and no greater than
            the platform ``size_t`` maximum. The actual number of ranges may
            be smaller when records are sparse.
        delimiter: The single raw byte marking record boundaries. Defaults to
            the newline byte ``b"\\n"`` (also accepted as the int ``10``).
        source: One of ``"mmap"`` (default, full-file mapping),
            ``"windowed"`` (bounded moving-window mapping), or ``"pread"``
            (positional reads, no mapping).
        window_bytes: Window size for ``source="windowed"``; must be
            >= 65536. ``None`` (default) uses the native 64 MiB default.
            Must be ``None`` for other sources.

    Returns:
        An immutable :class:`Plan`. No returned object references a live
        memory map.

    Raises:
        TypeError: Invalid argument types.
        ValueError: Non-positive ``parts``, invalid delimiter value, unknown
            source, out-of-range ``window_bytes``, misplaced ``window_bytes``,
            or an embedded NUL in the path.
        OverflowError: ``parts`` exceeds the platform ``size_t`` maximum.
        FileNotFoundError: The input file does not exist.
        IsADirectoryError: The input path is a directory.
        PlanningError: Native planning fails or a verified invariant is
            violated.
    """
    resolved = _coerce_path(path)
    parts = _coerce_parts(parts)
    delimiter_byte = _coerce_delimiter(delimiter)
    source_mode = _coerce_source(source)
    window_value = _coerce_window_bytes(window_bytes)
    if source != "windowed" and window_bytes is not None:
        raise ValueError(
            f"window_bytes requires source='windowed', got source={source!r}"
        )
    file_size = _verify_file(resolved)

    lib = _native.get_library()
    try:
        pairs = _native.plan_partition_ranges(
            lib,
            resolved,
            parts,
            bytes((delimiter_byte,)),
            source_mode,
            window_value,
        )
    except _native.NativeLibraryError as exc:
        raise PlanningError(f"native range planning failed: {exc}") from exc

    ranges = [
        Range(index, start, end, end - start)
        for index, (start, end) in enumerate(pairs)
    ]
    offset = 0
    for r in ranges:
        if r.start != offset:
            raise PlanningError(
                f"native plan has a gap or overlap at range {r.index}; "
                "the native library produced an inconsistent plan"
            )
        offset = r.end
    if offset != file_size:
        raise PlanningError(
            f"partition coverage {offset} does not match file size {file_size}; "
            "the native library produced an inconsistent plan"
        )
    _verify_record_alignment(resolved, tuple(ranges), delimiter_byte)

    return Plan(resolved, file_size, parts, delimiter_byte, tuple(ranges))
