# Performance baseline — 2026-08-14

## Decision

`CLI_IS_NEXT_HIGHEST_ROI`

The current core does not warrant windowed mmap, SIMD, allocation work, or API
changes. Normal record-aligned planning is sub-millisecond at 100 MiB and
low-single-digit milliseconds at 1 GiB. The measured slow path is long/sparse
delimiter search, where the implementation must examine large spans.

## Reconciliation and scope

* Benchmark branch base/final source SHA: `d8407d49e40bf39b90c259ffee6fd7f19681ab03`.
* Before branching, live GitHub showed PR #13 open draft at expected head
  `03753288e59ea7081f368e59c72c6a42372c5f8c`, all listed checks successful,
  and base `d8407d4...`. It was not modified.
* Work is isolated on `agent/performance-baseline-20260814`; no PR #13 code,
  public API/ABI, runtime dependency, or benchmark fixture is committed.

## Method and fixture matrix

`tests/performance_baseline.rs` is a std-only ignored integration benchmark.
It writes deterministic temporary fixtures under ignored `target/perf-fixtures`,
uses the public Rust API, validates coverage and record boundaries on every
sample, and deletes fixtures afterwards. Seven samples report median/min/max.

* `smoke`: 10 MiB, all shapes; `standard`: 10 and 100 MiB; `large`: 1 GiB
  JSONL and sparse-delimiter representatives only.
* Shapes: fixed 96-byte LF, uneven JSONL, 4 MiB records/no final LF, dense LF,
  8 MiB-spaced LF/no final LF, CRLF, `END_RECORD\0`, and absent `END_RECORD\0`.
* Operations: open/map, eager/lazy single and multi-byte planning, indexed
  retrieval, scalar full-file count reference, fixed plan/retrieval, and
  partitions N=1/2/4/8/16/32.

The planner jumps by requested chunk size and searches only from each target to
the next delimiter. Throughput uses the estimated post-target search bytes, not
logical file size; fixed/map/retrieval results are latency only.

## Environment and measurement limits

Windows 11 Home 10.0.22631, x86_64; Intel i7-8700 (6C/12T); 15.9 GiB RAM;
Rust release build; ~1.02 TiB free on D:. Files were generated just before
mapping, so results are warm-ish. Cache dropping was not attempted. A 1 GiB
run passed but had multi-second outliers (e.g. 2.805 ms median / 4.596 s max
JSONL planner; 518 ms median / 9.221 s max scalar count), so no timing gates
or false precision are proposed.

Peak RSS was not measured: the std-only harness lacks a portable RSS API and a
local Windows profiler exporter/analyzer was unavailable. Windows Performance
Recorder's CPU profile exists, but no opaque trace was retained without a
usable reader.

## Key results (median)

| Operation | 10 MiB | 100 MiB | 1 GiB | Finding |
|---|---:|---:|---:|---|
| Map only, JSONL | 0.094 ms | 0.109 ms | 0.109 ms | Setup is effectively size-independent. |
| Eager uneven JSONL | 0.025 ms / 0.001 MiB searched | 0.309 ms / 0.026 MiB | 2.805 ms / 0.269 MiB | Scales with chunks and tail distance, not bytes in file. |
| Lazy JSONL consume | 0.034 ms | 0.271 ms | 2.845 ms | Same planner work; O(1) state. |
| Scalar full-file LF count | 6.27 ms / 1.56 GiB/s | 54.60 ms / 1.79 GiB/s | 518.14 ms / 1.93 GiB/s | Linear full byte walk. |
| Eager sparse LF | 4.97 ms / 8 MiB | 47.53 ms / 87 MiB | 471.28 ms / 896 MiB | Near-full scanner cost. |
| Eager 4 MiB records | 3.58 ms / 7 MiB | 40.05 ms / 75 MiB | — | Same effect from oversized records. |
| Pattern absent | 5.27 ms / 9 MiB | 55.56 ms / 99 MiB | — | Multi-byte no-match is a full search. |
| CRLF/custom frequent pattern | 0.023–0.036 ms | 0.238–0.254 ms | — | Multi-byte matching is cheap when nearby. |
| Fixed plan/retrieve; indexed get | <0.001 ms | <0.001 ms | 0.002 ms | Not material. |

Partition planning at 100 MiB: JSONL is 0.006/0.023/0.092 ms at N=2/8/32;
4 MiB records are 0.969/7.299/33.479 ms; 8 MiB-spaced delimiters are
3.106/15.265/42.670 ms. Repeated target-to-next-record searches dominate the
pathological N scaling.

## Memory and bottleneck interpretation

An indexed range is 16 bytes: 1.6 KiB at 100 MiB/100 chunks and 16 KiB at
1 GiB/1,024 chunks with 1 MiB chunks. `ChunkCursor` is 40 bytes and
`PatternChunkCursor` 48 bytes. Eager mode preallocates one range `Vec`; fixed
mode is arithmetic. Metadata/allocation is not a measured normal-path cost.

`find_byte_swar` is the measured single-byte slow-path loop; pattern no-match
is `find_pattern_in_slice` (first-byte SWAR plus `starts_with`). The existing
scanner is already 8-byte SWAR. SIMD could help deliberate full scans around
1.6–2.0 GiB/s, but cannot materially change the normal path that examines only
kilobytes. Full-file mmap is not a measured throughput bottleneck: a 1 GiB map
opened in ~0.1 ms while sparse scan took 471 ms. It remains an address-space /
single-view limitation for much larger files, not a demonstrated current need.

## Regression and next-step scores

Retain benchmark code and this context report only. Keep smoke non-gating;
run standard/large manually or from a future `workflow_dispatch` workflow. Do
not retain timing JSON or create absolute CI gates until a controlled runner
demonstrates stability.

Scores are 0–5; lower is better for complexity, API risk, cross-platform impact,
and maintenance.

| Candidate | User value | Perf impact | Complexity | API risk | Cross-platform | Maintenance | Testability | Scope fit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| CLI | 5 | 0 | 3 | 0 | 3 | 3 | 5 | 4 |
| Windowed mmap | 2 | 0 | 5 | 3 | 5 | 5 | 2 | 3 |
| SIMD/scanner | 2 | 3 (pathological only) | 4 | 1 | 4 | 3 | 3 | 4 |
| Metadata/allocation | 1 | 0 | 2 | 1 | 2 | 2 | 4 | 4 |
| No core optimization yet | 4 | 5 | 0 | 0 | 0 | 0 | 5 | 5 |

CLI offers the highest user-facing ROI without disturbing the proven core.
Revisit scanner work only for confirmed long/sparse or absent-pattern workloads,
with symbol-capable profiling on the target platform.

## Validation and changed files

* Added `tests/performance_baseline.rs` and this report only.
* Passed `cargo test --release --test performance_baseline --no-run`, smoke,
  and standard runs. The 1 GiB test printed success in 59.88 s; the surrounding
  60-second command wrapper timed out during final teardown.
* No dependencies changed. No generated multi-GB data remains.
