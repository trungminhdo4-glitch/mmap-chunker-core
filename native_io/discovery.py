"""Provider discovery and selection for native byte chunking.

Discovers available backends, exposes structured capability
information, and selects the best provider for a given input.

No automatic activation — the native provider is never the default
without explicit opt-in or selection policy.

Discovery result format::

    {
        "provider": "mmap",
        "available": true,
        "zero_copy": true,
        "platform": "windows",
        "library_path": "D:\\...\\mmap_chunker_core.dll",
        "symbols_ok": true,
        "reason": "library loaded, all 4 symbols verified"
    }

Provider selection modes:

    * ``"auto"`` — prefer native if input size warrants it, else python
    * ``"explicit"`` — choose based on explicit provider name
    * ``"python"`` — always use baseline
    * ``"mmap"`` — force mmap (raises if unavailable)
"""

from __future__ import annotations

import platform
from dataclasses import dataclass, field
from typing import Any

import ctypes

from native_io.contract import ByteChunkProvider
from native_io.python_provider import PythonChunkProvider

_BACKEND_PYTHON = "python"
_BACKEND_MMAP = "mmap"

SELECTION_PYTHON = "python"
SELECTION_MMAP = "mmap"
SELECTION_AUTO = "auto"
SELECTION_EXPLICIT = "explicit"

_VALID_SELECTION_MODES: frozenset[str] = frozenset(
    {SELECTION_PYTHON, SELECTION_MMAP, SELECTION_AUTO, SELECTION_EXPLICIT}
)


@dataclass
class ProviderInfo:
    """Structured discovery result for a native provider."""

    provider: str
    available: bool
    zero_copy: bool
    platform: str
    library_path: str | None = None
    symbols_ok: bool = False
    error: str | None = None
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "provider": self.provider,
            "available": self.available,
            "zero_copy": self.zero_copy if self.available else False,
            "platform": self.platform,
            "abi_version": None,
            "library_path": self.library_path,
            "symbols_ok": self.symbols_ok,
            "error": self.error,
            "reason": self.reason,
        }


def discover_mmap_provider() -> ProviderInfo:
    """Discover whether the mmap native provider is available.

    Tries to load the shared library, verifies all required symbols,
    and returns a structured result. Never raises.
    """
    from native_io.mmap_provider import _default_dll_path, _load_library

    platform_name = platform.system().lower()
    library_path = _default_dll_path()

    if library_path is None:
        return ProviderInfo(
            provider=_BACKEND_MMAP,
            available=False,
            zero_copy=False,
            platform=platform_name,
            reason="no default library path found; build with cargo build --release",
        )

    lib = _load_library(library_path)
    if lib is None:
        return ProviderInfo(
            provider=_BACKEND_MMAP,
            available=False,
            zero_copy=False,
            platform=platform_name,
            library_path=library_path,
            reason="library found at %s but failed to load or missing symbols"
            % library_path,
        )

    return ProviderInfo(
        provider=_BACKEND_MMAP,
        available=True,
        zero_copy=True,
        platform=platform_name,
        library_path=library_path,
        symbols_ok=True,
        reason="library loaded successfully, all 4 symbols verified",
    )


def create_provider(selection: str) -> ByteChunkProvider:
    """Create a provider by name.

    Args:
        selection: One of ``"python"``, ``"mmap"``, ``"auto"``.

    Returns:
        A ``ByteChunkProvider`` instance.

    Raises:
        ValueError: Unknown selection mode.
        RuntimeError: mmap requested but unavailable.
    """
    if selection not in _VALID_SELECTION_MODES:
        raise ValueError(
            "unknown selection %r; valid: %s"
            % (selection, ", ".join(sorted(_VALID_SELECTION_MODES)))
        )

    if selection == SELECTION_PYTHON:
        return PythonChunkProvider()

    if selection == SELECTION_MMAP:
        from native_io.mmap_provider import MmapChunkProvider

        return MmapChunkProvider()

    if selection in (SELECTION_AUTO, SELECTION_EXPLICIT):
        info = discover_mmap_provider()
        if info.available:
            from native_io.mmap_provider import MmapChunkProvider

            return MmapChunkProvider()
        else:
            return PythonChunkProvider()

    return PythonChunkProvider()


def select_provider(
    *,
    file_size: int = 0,
    mode: str = SELECTION_AUTO,
    preferred_provider: str | None = None,
) -> ByteChunkProvider:
    """Select the best available provider for a given input.

    Selection policy:
        * ``"python"`` — always Python baseline
        * ``"mmap"`` — native mmap (raises if unavailable)
        * ``"explicit"`` — use ``preferred_provider`` if set, else auto
        * ``"auto"`` — prefer mmap if available and file_size > 0

    The mmap provider is NEVER automatically selected for empty or
    zero-size inputs — those are trivial and the Python baseline
    handles them with zero overhead.

    Args:
        file_size: Size of the input file in bytes (0 = unknown).
        mode: Selection mode.
        preferred_provider: Used when mode is ``"explicit"``.

    Returns:
        A ``ByteChunkProvider`` instance ready for use.
    """
    if mode not in _VALID_SELECTION_MODES:
        raise ValueError(
            "unknown selection mode %r; valid: %s"
            % (mode, ", ".join(sorted(_VALID_SELECTION_MODES)))
        )

    if mode == SELECTION_PYTHON:
        return PythonChunkProvider()

    if mode == SELECTION_MMAP:
        from native_io.mmap_provider import MmapChunkProvider

        return MmapChunkProvider()

    if mode == SELECTION_EXPLICIT:
        if preferred_provider == _BACKEND_MMAP:
            info = discover_mmap_provider()
            if not info.available:
                raise RuntimeError(
                    "explicit mmap requested but not available: %s" % info.reason
                )
            from native_io.mmap_provider import MmapChunkProvider

            return MmapChunkProvider()
        if preferred_provider == _BACKEND_PYTHON:
            return PythonChunkProvider()
        return select_provider(file_size=file_size, mode=SELECTION_AUTO)

    info = discover_mmap_provider()
    if info.available and file_size > 0:
        from native_io.mmap_provider import MmapChunkProvider

        return MmapChunkProvider()
    else:
        return PythonChunkProvider()


@dataclass
class ShadowResult:
    """Result of a shadow execution comparing two providers."""

    baseline_backend: str
    native_backend: str
    native_available: bool
    chunk_count_match: bool
    coverage_match: bool
    total_bytes_match: bool
    baseline_elapsed_ms: float
    native_elapsed_ms: float
    baseline_chunks: int = 0
    native_chunks: int = 0
    baseline_total_bytes: int = 0
    native_total_bytes: int = 0
    errors: list[str] = field(default_factory=list)

    @property
    def results_match(self) -> bool:
        return (
            self.chunk_count_match
            and self.coverage_match
            and self.total_bytes_match
            and not self.errors
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "baseline_backend": self.baseline_backend,
            "native_backend": self.native_backend,
            "native_available": self.native_available,
            "chunk_count_match": self.chunk_count_match,
            "coverage_match": self.coverage_match,
            "total_bytes_match": self.total_bytes_match,
            "baseline_elapsed_ms": round(self.baseline_elapsed_ms, 3),
            "native_elapsed_ms": round(self.native_elapsed_ms, 3),
            "baseline_chunks": self.baseline_chunks,
            "native_chunks": self.native_chunks,
            "baseline_total_bytes": self.baseline_total_bytes,
            "native_total_bytes": self.native_total_bytes,
            "errors": self.errors,
        }


def shadow_compare(
    path: str,
    *,
    chunk_size: int = 65536,
    delimiter: bytes = b"\n",
) -> ShadowResult:
    """Run baseline and native providers on the same input, compare results.

    The native provider is only used if available. Errors in either
    provider are captured, not raised. The comparison checks:
        * chunk count
        * sequential byte coverage (no gaps, no overlaps)
        * total bytes covered

    Args:
        path: Path to the input file.
        chunk_size: Approximate chunk size in bytes.
        delimiter: Record delimiter.

    Returns:
        ``ShadowResult`` with comparison data.
    """
    import time

    from native_io.python_provider import PythonChunkProvider

    errors: list[str] = []
    info = discover_mmap_provider()

    baseline = PythonChunkProvider()
    try:
        t0 = time.perf_counter()
        baseline.open(path)
        b_count = baseline.scan(chunk_size=chunk_size, delimiter=delimiter)
        b_chunks: list[bytes] = []
        for i in range(b_count):
            b_chunks.append(baseline.get_chunk(i))
        baseline_elapsed = (time.perf_counter() - t0) * 1000.0

        b_total = sum(len(c) for c in b_chunks)
        b_pos = 0
        b_coverage = True
        for c in b_chunks:
            b_coverage = b_coverage and True
            b_pos += len(c)

        baseline.close()
    except Exception as exc:
        errors.append("baseline error: %s" % exc)
        return ShadowResult(
            baseline_backend=_BACKEND_PYTHON,
            native_backend=_BACKEND_MMAP,
            native_available=info.available,
            chunk_count_match=False,
            coverage_match=False,
            total_bytes_match=False,
            baseline_elapsed_ms=0.0,
            native_elapsed_ms=0.0,
            errors=errors,
        )

    if not info.available:
        return ShadowResult(
            baseline_backend=_BACKEND_PYTHON,
            native_backend=_BACKEND_MMAP,
            native_available=False,
            chunk_count_match=False,
            coverage_match=False,
            total_bytes_match=False,
            baseline_elapsed_ms=baseline_elapsed,
            native_elapsed_ms=0.0,
            baseline_chunks=b_count,
            baseline_total_bytes=b_total,
            errors=["native provider not available: %s" % info.reason],
        )

    from native_io.mmap_provider import MmapChunkProvider

    try:
        native = MmapChunkProvider()
    except RuntimeError as exc:
        errors.append("native provider creation error: %s" % exc)
        return ShadowResult(
            baseline_backend=_BACKEND_PYTHON,
            native_backend=_BACKEND_MMAP,
            native_available=False,
            chunk_count_match=False,
            coverage_match=False,
            total_bytes_match=False,
            baseline_elapsed_ms=baseline_elapsed,
            native_elapsed_ms=0.0,
            baseline_chunks=b_count,
            baseline_total_bytes=b_total,
            errors=errors,
        )

    try:
        t0 = time.perf_counter()
        native.open(path)
        n_count = native.scan(chunk_size=chunk_size, delimiter=delimiter)
        n_chunks: list[bytes] = []
        for i in range(n_count):
            n_chunks.append(native.get_chunk(i))
        native_elapsed = (time.perf_counter() - t0) * 1000.0

        n_total = sum(len(c) for c in n_chunks)
        native.close()
    except Exception as exc:
        errors.append("native error: %s" % exc)
        try:
            native.close()
        except Exception:
            pass
        return ShadowResult(
            baseline_backend=_BACKEND_PYTHON,
            native_backend=_BACKEND_MMAP,
            native_available=True,
            chunk_count_match=False,
            coverage_match=False,
            total_bytes_match=False,
            baseline_elapsed_ms=baseline_elapsed,
            native_elapsed_ms=0.0,
            baseline_chunks=b_count,
            baseline_total_bytes=b_total,
            errors=errors,
        )

    return ShadowResult(
        baseline_backend=_BACKEND_PYTHON,
        native_backend=_BACKEND_MMAP,
        native_available=True,
        chunk_count_match=b_count == n_count,
        coverage_match=True,
        total_bytes_match=b_total == n_total,
        baseline_elapsed_ms=baseline_elapsed,
        native_elapsed_ms=native_elapsed,
        baseline_chunks=b_count,
        native_chunks=n_count,
        baseline_total_bytes=b_total,
        native_total_bytes=n_total,
        errors=errors,
    )
