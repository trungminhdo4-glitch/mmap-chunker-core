"""native_io — Agnostic native byte chunking layer.

Provides a minimal capability contract for chunked byte access
to files, a portable Python baseline, and a native mmap provider
(optional, requires ``mmap_chunker_core`` shared library).

Usage::

    from native_io import select_provider

    with select_provider(file_size=path.stat().st_size) as p:
        p.open(str(path))
        p.scan(chunk_size=65536)
        for i in range(p.chunk_count):
            chunk = p.get_chunk(i)
"""

from native_io.contract import ByteChunkProvider
from native_io.discovery import (
    ProviderInfo,
    ShadowResult,
    create_provider,
    discover_mmap_provider,
    select_provider,
    shadow_compare,
)
from native_io.plan_manifest import (
    Plan,
    PlanError,
    PlanFormatError,
    PlanIdentityMismatch,
    Range,
    load_plan,
    load_verified_plan,
    parse_manifest,
    verify_identity,
    verify_plan,
    verify_ranges,
)
from native_io.python_provider import PythonChunkProvider
from native_io.record_index import (
    RecordIndex,
    load_index,
    load_verified_index,
    parse_index,
    plan_partition_boundaries_from_index,
    verify_index,
)

__all__ = [
    "ByteChunkProvider",
    "Plan",
    "PlanError",
    "PlanFormatError",
    "PlanIdentityMismatch",
    "PythonChunkProvider",
    "ProviderInfo",
    "Range",
    "RecordIndex",
    "ShadowResult",
    "create_provider",
    "discover_mmap_provider",
    "load_index",
    "load_plan",
    "load_verified_index",
    "load_verified_plan",
    "parse_index",
    "parse_manifest",
    "plan_partition_boundaries_from_index",
    "select_provider",
    "shadow_compare",
    "verify_identity",
    "verify_index",
    "verify_plan",
    "verify_ranges",
]
