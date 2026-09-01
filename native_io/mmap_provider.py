"""Native mmap chunk provider via the mmap-chunker-core C ABI.

Wraps the Rust ``mmap_chunker_core`` shared library using ``ctypes``.
Provides zero-copy OS-level access (memory-mapped I/O) with a safe
Python API that returns ``bytes`` copies for each chunk.

The shared library is discovered via:

    1. ``MMAP_CHUNKER_DLL`` environment variable (full path)
    2. Default location relative to this package::

           ../target/release/mmap_chunker_core.dll  (Windows)
           ../target/release/libmmap_chunker_core.so (Unix)

If the library cannot be loaded, this provider is unavailable
and the harness falls back to the Python baseline.
"""

from __future__ import annotations

import ctypes
import os
from ctypes import (
    POINTER,
    Structure,
    byref,
    c_char_p,
    c_int,
    c_size_t,
    c_void_p,
)
from pathlib import Path

from native_io.contract import ByteChunkProvider


_PACKAGE_DIR = Path(__file__).resolve().parent
_PROJECT_ROOT = _PACKAGE_DIR.parent


def _default_dll_path() -> str | None:
    """Return the default library path or None."""
    candidates: list[Path] = [
        _PROJECT_ROOT / "target" / "release" / "mmap_chunker_core.dll",
        _PROJECT_ROOT / "target" / "release" / "libmmap_chunker_core.so",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    return None


def _load_library(path: str | None = None) -> ctypes.CDLL | None:
    """Load the mmap-chunker-core shared library.

    Returns None if the library cannot be loaded.
    """
    resolved = path or os.environ.get("MMAP_CHUNKER_DLL") or _default_dll_path()
    if resolved is None:
        return None
    try:
        lib = ctypes.CDLL(str(resolved))
        _ = lib.mmap_engine_open
        _ = lib.mmap_engine_scan_chunks
        _ = lib.mmap_engine_get_chunk
        _ = lib.mmap_engine_free
    except (OSError, AttributeError):
        return None
    return lib


class _CChunkView(Structure):
    """Matches ``CChunkView`` from ``mmap_chunker.h``.

    Layout (64-bit):

        offset  field   type    size
        ------  -----   ----    ----
        0       data    void*   8
        8       len     size_t  8
        total: 16 bytes
    """

    _fields_ = [
        ("data", c_void_p),
        ("len", c_size_t),
    ]


class MmapChunkProvider:
    """Native chunk provider using mmap via the C ABI.

    Usage::

        with MmapChunkProvider() as p:
            p.open("records.jsonl")
            p.scan(chunk_size=64 * 1024)
            for i in range(p.chunk_count):
                chunk = p.get_chunk(i)

    ``delimiter`` is always ``b"\\n"`` — the current ABI only
    supports newline-delimited records.
    """

    backend: str
    zero_copy: bool
    _lib: ctypes.CDLL | None
    _handle: ctypes.c_void_p
    _chunk_count: int

    def __init__(self, library: ctypes.CDLL | None = None) -> None:
        self.backend = "mmap"
        self.zero_copy = True
        self._lib = library or _load_library()
        if self._lib is None:
            raise RuntimeError(
                "mmap_chunker_core shared library not found. "
                "Set MMAP_CHUNKER_DLL environment variable or build "
                "with `cargo build --release` from the project root."
            )

        self._lib.mmap_engine_open.argtypes = [c_char_p]
        self._lib.mmap_engine_open.restype = c_void_p

        self._lib.mmap_engine_scan_chunks.argtypes = [c_void_p, c_size_t]
        self._lib.mmap_engine_scan_chunks.restype = c_size_t

        self._lib.mmap_engine_get_chunk.argtypes = [
            c_void_p,
            c_size_t,
            POINTER(_CChunkView),
        ]
        self._lib.mmap_engine_get_chunk.restype = c_int

        self._lib.mmap_engine_free.argtypes = [c_void_p]
        self._lib.mmap_engine_free.restype = None

        self._handle = c_void_p()
        self._chunk_count = 0

    @property
    def chunk_count(self) -> int:
        return self._chunk_count

    def open(self, path: str) -> None:
        """Open and memory-map the file at ``path``."""
        self.close()
        encoded = str(Path(path).resolve()).encode("utf-8")
        result = self._lib.mmap_engine_open(encoded)  # type: ignore[union-attr]
        if not result:
            raise OSError("mmap_engine_open failed for path: %s" % path)
        self._handle = result

    def scan(
        self,
        *,
        chunk_size: int = 65536,
        delimiter: bytes = b"\n",
    ) -> int:
        """Scan for chunk boundaries.

        Only ``delimiter=b"\\n"`` is supported by the current C ABI.
        """
        if delimiter != b"\n":
            raise ValueError(
                'MmapChunkProvider only supports newline delimiter (b"\\n"), '
                "got %r" % delimiter
            )
        if not self._handle:
            raise RuntimeError("open() must be called before scan()")
        count = self._lib.mmap_engine_scan_chunks(  # type: ignore[union-attr]
            self._handle,
            c_size_t(chunk_size),
        )
        self._chunk_count = count
        return count

    def get_chunk(self, index: int) -> bytes:
        """Return a ``bytes`` copy of the chunk at ``index``."""
        if index < 0 or index >= self._chunk_count:
            raise IndexError(
                "chunk index %d out of range [0, %d)" % (index, self._chunk_count)
            )
        view = _CChunkView()
        ret = self._lib.mmap_engine_get_chunk(  # type: ignore[union-attr]
            self._handle,
            c_size_t(index),
            byref(view),
        )
        if ret != 0:
            raise RuntimeError(
                "mmap_engine_get_chunk failed at index %d (returned %d)" % (index, ret)
            )
        if not view.data or view.len == 0:
            return b""

        buf = (ctypes.c_uint8 * view.len).from_address(view.data)
        return bytes(buf)

    def close(self) -> None:
        """Unmap and release all resources."""
        if self._handle:
            self._lib.mmap_engine_free(self._handle)  # type: ignore[union-attr]
            self._handle = c_void_p()
        self._chunk_count = 0

    def __enter__(self) -> MmapChunkProvider:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()
