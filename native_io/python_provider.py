"""Python stdlib-only baseline provider.

Reference implementation of ``ByteChunkProvider`` using only the
Python standard library. Always available, zero dependencies.

Serves as:

    * Portable fallback when native providers are unavailable
    * Correctness oracle for native provider testing
    * Reference for future backend implementers

Strategy: read the file into memory, scan for boundaries in pure
Python using the same algorithm as the Rust scanner. Returns a
``bytes`` copy for each chunk.
"""

from __future__ import annotations

from pathlib import Path


_EMPTY_CHUNKS: list[tuple[int, int]] = []


def _find_boundaries(
    data: bytes,
    chunk_size: int,
    delimiter: bytes,
) -> list[tuple[int, int]]:
    """Find chunk boundaries in ``data`` using the given delimiter.

    Same algorithm as ``scanner::find_chunk_boundaries`` in Rust:
    sequential chunks covering the entire input, each boundary placed
    after a delimiter found at or after the target step offset.

    Returns a list of ``(start, end)`` absolute byte offset pairs.
    """
    if len(delimiter) != 1:
        raise ValueError("delimiter must be a single byte, got %r" % delimiter)
    if not data:
        return _EMPTY_CHUNKS

    delimiter_byte = delimiter[0]
    length = len(data)
    step = max(chunk_size, 1)
    estimate = (length // step) + 2
    chunks: list[tuple[int, int]] = []
    start = 0

    while start < length:
        end = start + step

        if end >= length:
            end = length
        else:
            remainder = data[end:]
            try:
                rel_pos = remainder.index(delimiter_byte)
            except ValueError:
                end = length
            else:
                end = end + rel_pos + 1
                if end > length:
                    end = length

        chunks.append((start, end))
        start = end

    return chunks


class PythonChunkProvider:
    """Baseline chunk provider using Python stdlib only.

    Usage::

        with PythonChunkProvider() as p:
            p.open("records.jsonl")
            p.scan(chunk_size=64 * 1024)
            for i in range(p.chunk_count):
                chunk = p.get_chunk(i)
    """

    backend: str
    zero_copy: bool
    _path: str | None
    _data: bytes
    _boundaries: list[tuple[int, int]]

    def __init__(self) -> None:
        self.backend = "python"
        self.zero_copy = False
        self._path = None
        self._data = b""
        self._boundaries = _EMPTY_CHUNKS

    @property
    def chunk_count(self) -> int:
        return len(self._boundaries)

    def open(self, path: str) -> None:
        """Read the file at ``path`` fully into memory."""
        self.close()
        resolved = Path(path).resolve()
        self._path = str(resolved)
        self._data = resolved.read_bytes()

    def scan(
        self,
        *,
        chunk_size: int = 65536,
        delimiter: bytes = b"\n",
    ) -> int:
        """Scan for chunk boundaries in the loaded data."""
        if self._path is None:
            raise RuntimeError("open() must be called before scan()")
        self._boundaries = _find_boundaries(self._data, chunk_size, delimiter)
        return len(self._boundaries)

    def get_chunk(self, index: int) -> bytes:
        """Return a ``bytes`` copy of the chunk at ``index``."""
        if index < 0 or index >= len(self._boundaries):
            raise IndexError(
                "chunk index %d out of range [0, %d)" % (index, len(self._boundaries))
            )
        start, end = self._boundaries[index]
        return self._data[start:end]

    def chunk_bounds(self, index: int) -> tuple[int, int]:
        """Return the ``(start, end)`` byte offsets of the chunk at ``index``."""
        if index < 0 or index >= len(self._boundaries):
            raise IndexError(
                "chunk index %d out of range [0, %d)" % (index, len(self._boundaries))
            )
        return self._boundaries[index]

    def close(self) -> None:
        """Release all resources."""
        self._path = None
        self._data = b""
        self._boundaries = _EMPTY_CHUNKS

    def __enter__(self) -> PythonChunkProvider:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()
