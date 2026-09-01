"""Minimal agnostic contract for chunked byte access to files.

Defines the ``ByteChunkProvider`` protocol that any backend (Python
stdlib, mmap via Rust, future native accelerators) must satisfy.

The contract is deliberately small and optional: no inheritance
required, no plugin registry, no framework. Any object with the
required attributes and methods satisfies it via structural typing.

Design properties:

    * Language-neutral semantics — no Rust, no mmap terms
    * Backend-exchangeable — same contract for all providers
    * Zero-copy is a *property*, not a requirement — Python may copy
    * Singleton lifecycle — open → scan → iterate → close
    * Immutable input — file is read-only for the provider lifetime
    * Deterministic — repeated scans on same input produce same results
    * Coverage — chunks partition the file exactly (no gaps, no overlaps)
    * Newline delimiter default — matches common record formats (JSONL, CSV, logs)
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable


@runtime_checkable
class ByteChunkProvider(Protocol):
    """Structural protocol for chunked byte access to a file.

    A provider opens a file, scans it for record boundaries at
    approximately `chunk_size` byte intervals, and allows indexed
    access to individual chunks. The last chunk always extends to
    the end of the file.

    Chunks partition the file exactly: no gaps, no overlaps.
    Boundaries are placed after delimiter bytes found at or after
    the target offset. If no delimiter exists in the remainder,
    the remainder becomes one chunk extending to EOF.

    Properties:

        backend : str
            Provider identifier ("python", "mmap", etc.)

        zero_copy : bool
            True if the underlying access avoids copying at the OS
            level (e.g. memory-mapped I/O). The Python API still
            returns a ``bytes`` copy for safety.

        chunk_count : int
            Number of chunks found by the last ``scan()`` call.

    Methods:

        open(path) -> None
            Open the file at ``path`` for read-only access.

        scan(*, chunk_size, delimiter) -> int
            Scan for chunk boundaries. Returns chunk count.

        get_chunk(index) -> bytes
            Return the chunk at ``index`` as a ``bytes`` object.

        close() -> None
            Release all resources. Chunks are invalid after this.
    """

    @property
    def backend(self) -> str: ...

    @property
    def zero_copy(self) -> bool: ...

    @property
    def chunk_count(self) -> int: ...

    def open(self, path: str) -> None: ...

    def scan(
        self,
        *,
        chunk_size: int = 65536,
        delimiter: bytes = b"\n",
    ) -> int: ...

    def get_chunk(self, index: int) -> bytes: ...

    def close(self) -> None: ...
