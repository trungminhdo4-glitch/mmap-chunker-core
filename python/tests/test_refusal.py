"""Cross-surface refusal matrix for non-regular inputs (Python surface).

Covers the wheel's planning entry point: refusal must happen before any
native call, matching the probe's fail-closed contract.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

_REPO = Path(__file__).resolve().parents[2]
_PYTHON_SRC = _REPO / "python"

# Prefer the installed package; fall back to the repository source tree.
try:
    import mmap_chunker  # noqa: F401

    _FROM_SOURCE = False
except ImportError:
    sys.path.insert(0, str(_PYTHON_SRC))
    import mmap_chunker  # noqa: F401

    _FROM_SOURCE = True

from mmap_chunker import plan_file  # noqa: E402


def test_plan_file_missing_path(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        plan_file(tmp_path / "nope.dat", 2)
