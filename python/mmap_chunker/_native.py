"""Deterministic loading of the bundled native shared library.

The shared library ships inside the installed package at
``mmap_chunker/_native/``. It is located relative to this module (never via
PATH, LD_LIBRARY_PATH, DYLD_LIBRARY_PATH, subprocess lookup, or download).
The ABI version and the record-partitioning capability are validated at load
time and failures are reported as actionable exceptions.
"""

from __future__ import annotations

import ctypes
import os
import platform
from pathlib import Path
from typing import Callable, TypeVar

ABI_VERSION = 0x0001_0003
ABI_VERSION_TEXT = "v1.3"

CAP_ZERO_COPY = 1 << 0
CAP_CONFIGURABLE_DELIMITER = 1 << 1
CAP_ERROR_STRINGS = 1 << 2
CAP_FIXED_SIZE_CHUNKING = 1 << 3
CAP_RECORD_PARTITIONING = 1 << 4
CAP_MULTI_BYTE_DELIMITER = 1 << 5

_REQUIRED_CAPABILITY = CAP_RECORD_PARTITIONING

_LIBRARY_NAMES = {
    ("linux", "x86_64"): "libmmap_chunker_core.so",
    ("linux", "aarch64"): "libmmap_chunker_core.so",
    ("linux", "arm64"): "libmmap_chunker_core.so",
    ("darwin", "x86_64"): "libmmap_chunker_core.dylib",
    ("darwin", "arm64"): "libmmap_chunker_core.dylib",
    ("win32", "AMD64"): "mmap_chunker_core.dll",
}


class NativeLibraryError(RuntimeError):
    """Raised when the bundled native library cannot be loaded or is invalid."""


class _CChunkView(ctypes.Structure):
    """Matches the C ``CChunkView`` layout (data ptr + size_t len, 16 bytes)."""

    _fields_ = [
        ("data", ctypes.POINTER(ctypes.c_uint8)),
        ("len", ctypes.c_size_t),
    ]


def _platform_key() -> tuple[str, str]:
    system = platform.system().lower()
    machine = platform.machine()
    if system == "linux" and machine == "x86_64":
        return ("linux", "x86_64")
    if system == "linux" and machine in ("aarch64", "arm64"):
        return ("linux", "aarch64")
    if system == "darwin" and machine in ("x86_64", "AMD64"):
        return ("darwin", "x86_64")
    if system == "darwin" and machine in ("arm64", "aarch64"):
        return ("darwin", "arm64")
    if system == "windows" and machine in ("AMD64", "x86_64"):
        return ("win32", "AMD64")
    return (system, machine)


def _library_name() -> str:
    key = _platform_key()
    try:
        return _LIBRARY_NAMES[key]
    except KeyError:
        system, machine = key
        raise NativeLibraryError(
            "no bundled native library for platform/architecture "
            f"{system!r}/{machine!r}. Supported platforms: Linux x86_64, "
            "Linux aarch64, macOS x86_64, macOS arm64, Windows x86_64. "
            "The native ABI is 64-bit only."
        ) from None


def library_path() -> Path:
    """Absolute path to the bundled native library inside the package."""
    package_dir = Path(__file__).resolve().parent
    candidate = package_dir / "_native" / _library_name()
    if not candidate.is_file():
        raise NativeLibraryError(
            f"bundled native library not found at {candidate}. "
            "The installed wheel does not match this platform/architecture, "
            "or the package was not installed from a matching wheel. "
            "Reinstall with a wheel built for this platform (see the "
            "platform matrix in the distribution documentation)."
        )
    return candidate


def _configure(lib: ctypes.CDLL) -> None:
    lib.mmap_engine_abi_version.argtypes = []
    lib.mmap_engine_abi_version.restype = ctypes.c_uint32
    lib.mmap_engine_capabilities.argtypes = []
    lib.mmap_engine_capabilities.restype = ctypes.c_uint32
    lib.mmap_engine_last_error.argtypes = []
    lib.mmap_engine_last_error.restype = ctypes.c_char_p
    lib.mmap_engine_open.argtypes = [ctypes.c_char_p]
    lib.mmap_engine_open.restype = ctypes.c_void_p
    lib.mmap_engine_partition_records.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_uint8,
    ]
    lib.mmap_engine_partition_records.restype = ctypes.c_size_t
    lib.mmap_engine_get_chunk.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(_CChunkView),
    ]
    lib.mmap_engine_get_chunk.restype = ctypes.c_int32
    lib.mmap_engine_free.argtypes = [ctypes.c_void_p]
    lib.mmap_engine_free.restype = None


def _load(path: Path) -> ctypes.CDLL:
    try:
        if os.name == "nt":
            # Windows DLL search semantics: put the bundled directory on the
            # DLL search path so co-located runtime dependencies resolve. The
            # library itself is still loaded by absolute path.
            add_dll_directory = getattr(os, "add_dll_directory", None)
            if add_dll_directory is not None:
                add_dll_directory(str(path.parent))
        lib = ctypes.CDLL(str(path))
    except OSError as exc:
        raise NativeLibraryError(
            f"failed to load bundled native library {path}: {exc}. "
            "On Windows ensure the required VC++ runtime is installed "
            "(Visual C++ Redistributable)."
        ) from exc
    _configure(lib)
    return lib


def _validate(lib: ctypes.CDLL) -> None:
    try:
        abi = int(lib.mmap_engine_abi_version())
        capabilities = int(lib.mmap_engine_capabilities())
    except (AttributeError, OSError) as exc:
        raise NativeLibraryError(
            f"bundled native library is not a mmap-chunker-core library: {exc}"
        ) from exc
    if abi != ABI_VERSION:
        raise NativeLibraryError(
            f"ABI mismatch: bundled library reports ABI 0x{abi:08x}, "
            f"expected 0x{ABI_VERSION:08x} ({ABI_VERSION_TEXT}). "
            "Install a wheel whose native library matches this Python package."
        )
    if not capabilities & _REQUIRED_CAPABILITY:
        raise NativeLibraryError(
            "bundled native library lacks the RECORD_PARTITIONING capability "
            f"(capabilities=0x{capabilities:08x}), which plan_file requires."
        )


T = TypeVar("T")
_lib: ctypes.CDLL | None = None


def get_library() -> ctypes.CDLL:
    """Load (once) and validate the bundled native library."""
    global _lib
    if _lib is None:
        loaded = _load(library_path())
        _validate(loaded)
        _lib = loaded
    return _lib


def last_error(lib: ctypes.CDLL) -> str:
    """Read the thread-local native error string (empty when none)."""
    raw = lib.mmap_engine_last_error()
    if not raw:
        return ""
    return raw.decode("utf-8", "replace")
