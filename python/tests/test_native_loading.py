"""Tests for the deterministic native library loader in mmap_chunker._native."""

from __future__ import annotations

import ctypes
import sys
from pathlib import Path

import pytest

_REPO = Path(__file__).resolve().parents[2]
_PYTHON_SRC = _REPO / "python"

try:
    import mmap_chunker  # noqa: F401
except ImportError:
    sys.path.insert(0, str(_PYTHON_SRC))
    import mmap_chunker  # noqa: F401

from mmap_chunker import _native  # noqa: E402


def test_library_path_is_inside_package() -> None:
    path = _native.library_path()
    pkg_dir = Path(mmap_chunker.__file__).resolve().parent
    assert path.is_file()
    assert path.parent == pkg_dir / "_native"


def test_loader_returns_singleton_cdll() -> None:
    lib = _native.get_library()
    assert isinstance(lib, ctypes.CDLL)
    assert _native.get_library() is lib


def test_abi_version_matches_expected() -> None:
    lib = _native.get_library()
    assert int(lib.mmap_engine_abi_version()) == 0x0001_0003


def test_required_capability_present() -> None:
    lib = _native.get_library()
    caps = int(lib.mmap_engine_capabilities())
    assert caps & _native.CAP_RECORD_PARTITIONING
    assert caps & _native.CAP_ZERO_COPY


def test_native_symbols_available() -> None:
    lib = _native.get_library()
    for symbol in (
        "mmap_engine_open",
        "mmap_engine_partition_records",
        "mmap_engine_get_chunk",
        "mmap_engine_free",
        "mmap_engine_last_error",
        "mmap_engine_abi_version",
        "mmap_engine_capabilities",
    ):
        assert getattr(lib, symbol, None) is not None, symbol


def test_error_paths_are_actionable(tmp_path: Path) -> None:
    import mmap_chunker.planning as planning

    with pytest.raises(FileNotFoundError):
        planning.plan_file(tmp_path / "nope.jsonl", parts=4)
    with pytest.raises(IsADirectoryError):
        planning.plan_file(tmp_path, parts=4)


def test_unsupported_platform_raises_clear_error() -> None:
    import mmap_chunker._native as native

    original_key = native._platform_key
    native._platform_key = lambda: ("plan9", "mips")  # type: ignore[assignment]
    try:
        with pytest.raises(native.NativeLibraryError) as exc_info:
            native._library_name()
        message = str(exc_info.value)
        assert "64-bit" in message
        assert "x86_64" in message
    finally:
        native._platform_key = original_key  # type: ignore[assignment]
