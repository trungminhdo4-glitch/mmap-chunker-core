"""mmap_chunker — record-aligned byte-range planning for immutable local files.

A zero-dependency Python package that drives the stable mmap-chunker-core C
ABI through stdlib ctypes. The native shared library ships inside the wheel;
no Rust toolchain, CLI, or manual library placement is required at runtime.

Public API::

    from mmap_chunker import plan_file

    plan = plan_file("records.jsonl", parts=8)
    for r in plan.ranges:
        print(r.start, r.end, r.length)

Diagnostics::

    import mmap_chunker

    mmap_chunker.__version__
    mmap_chunker.abi_version()   # 0x00010003 (v1.3)
    mmap_chunker.capabilities()  # native capability bitmask

Optional DataTrove integration (requires the ``[datatrove]`` extra)::

    from mmap_chunker import plan_file
    from mmap_chunker.integrations.datatrove import RangeJsonlReader

    plan = plan_file(path, parts=4)
    reader = RangeJsonlReader(path, plan)
"""

from __future__ import annotations

from mmap_chunker import _native
from mmap_chunker.planning import (
    DEFAULT_DELIMITER,
    Plan,
    PlanningError,
    Range,
    plan_file,
)

__all__ = [
    "DEFAULT_DELIMITER",
    "Plan",
    "PlanningError",
    "Range",
    "plan_file",
    "abi_version",
    "capabilities",
]

__version__ = "0.2.4"


def abi_version() -> int:
    """Return the native ABI version as ``(major << 16) | minor``.

    The bundled library must report ABI 0x00010003 (v1.3); loading it also
    validates this requirement.
    """
    return int(_native.get_library().mmap_engine_abi_version())


def capabilities() -> int:
    """Return the native capability bitmask.

    Bit 4 (RECORD_PARTITIONING) is required by :func:`plan_file`. Additional
    capability bits may be present in newer libraries without affecting
    compatibility.
    """
    return int(_native.get_library().mmap_engine_capabilities())
