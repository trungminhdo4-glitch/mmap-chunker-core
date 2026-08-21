# Changelog

## [Unreleased]

## [0.2.6] — 2026-08-20

### Fixed

- DataTrove `RangeJsonlReader` now honors `skip` and `limit`.
- Range processing no longer materializes an entire worker partition in
  Python memory.
- Global line-offset construction now scans in bounded chunks instead of
  copying large mmap slices.
- Plan/executor world-size mismatches now fail explicitly.

### Validation

- Differential parity against DataTrove 0.10.0.
- Real `LocalPipelineExecutor` single-file multi-rank proof.
- No-loss/no-duplicate range coverage.

## [0.2.5] — 2026-08-19

### Added

- Python distribution `mmap-chunker-core` on PyPI: `py3-none-<platform>`
  wheels for Linux x86_64/aarch64 (`manylinux_2_17_*`), macOS x86_64/arm64,
  and Windows x86_64 (`win_amd64`), plus an sdist that rebuilds the native
  library with Cargo.
- Trusted-publishing release automation (OIDC) for PyPI and crates.io behind
  protected GitHub environments; a tag push builds, verifies, and publishes
  the exact release artifact set, while `workflow_dispatch` remains a
  non-publishing dry run.

### Distribution / adoption

- The distribution version `0.2.5` matches the crate version; the Python
  distribution is additive and adoption-focused. Production Rust algorithms
  and the C ABI are unchanged.

### Compatibility

- C ABI remains `0x0001_0003` (v1.3).
- Capability bitmask remains `0x3f`.
- Exported `mmap_engine_*` symbol contract unchanged.
- Rust MSRV remains 1.77.
- Existing CLI behavior unchanged.

## [0.2.4] — 2026-08-15

### Added

- Ordered multi-file logical dataset partitioning via `partition-files`.
- Deterministic five-field TSV worker/source-local ranges with explicit source
  ordering; duplicate paths are separate source entries.
- Empty-source and all-empty dataset handling, raw single-byte delimiters,
  record-aligned worker boundaries, and compact worker indices when requested
  targets collapse.

### Compatibility

- The existing single-file `partition` contract remains unchanged.
- No Rust API, C ABI, dependency, or MSRV changes.

## [0.2.3] — 2026-08-14

### Added

- Optional `--worker K` output selection for one zero-based CLI partition.
- Optional `--delimiter-byte B` framing with raw byte values from 0 to 255.
- Standalone five-platform CLI archives with matching SHA-256 checksums.
- `cargo-binstall` release metadata for the standalone CLI assets.

### Changed

- The release workflow now verifies standalone CLI artifacts alongside native-library artifacts.

### Release validation

- Exact archive file sets, runtime CLI smoke, and Linux CLI GLIBC ceiling/runtime proof; existing native ABI and consumer guarantees remain preserved.

## [0.2.2] — 2026-08-14

### Added

- `mmap-chunker partition FILE --parts N` CLI for deterministic, contiguous,
  newline-record-aligned byte ranges emitted as numeric TSV without a header.
- A dependency-free JSONL multiprocessing proof for independent local workers.

### Changed

- Repositioned the documentation around record-aligned byte-range planning for
  immutable local files and clarified that delimiters provide raw framing, not
  CSV/JSON parsing.
- Centralized internal chunk-plan state without changing the public Rust or C
  ABI surfaces.

### Fixed

- Corrected the POSIX `open` declaration and call shape for the variadic mode
  argument contract.
- Bounded extreme record-partition requests and documented the resulting
  non-empty partition limit.

### Release validation

- Added independent scanner differential oracles, bounded fuzz smoke targets,
  focused ASan FFI coverage, and exact C ABI/package-content checks.
- Added Linux C/Python/Go/cgo/C# conformance against one native library and
  enforced the x86_64 GNU/Linux GLIBC_2.17 packaged-artifact/runtime contract.
- Added a decision-grade performance baseline; no scanner optimization or
  production dependency was introduced.

## [0.2.1] — 2026-08-09

### Added

- C ABI multi-byte delimiter scanning via `mmap_engine_scan_chunks_pattern`.
  The delimiter is a borrowed pointer-plus-length byte range, is validated
  before scanning, and is not retained by the engine. Added ABI v1.3 and
  `CAP_MULTI_BYTE_DELIMITER` (bit 5).
- Safe Rust API (`MmapChunker`) with `Path`/`OsStr` support wrapping `MmapFile`
  with a chunk-layout state machine (`Empty`, `Delimited`, `Fixed`, `Partitioned`)
- Lazy streaming `ChunkCursor` — O(1) memory (~40 bytes on 64-bit) delimiter-aware
  chunk traversal without pre-computing all boundaries
- `PatternChunkCursor` — lazy multi-byte delimiter cursor (e.g., `b"\r\n"`, `b"\r\n\r\n"`)
- Multi-byte delimiter support via `find_chunk_boundaries_pattern` and `find_pattern_in_slice`
  (Rust-only, first-byte SWAR + `starts_with` verification)

### Changed

- Native release archives now cover Linux x86_64, Linux aarch64 package-only,
  Windows x86_64, macOS x86_64, and macOS aarch64, with checksums and
  extracted-archive ABI/consumer verification for native lanes.
- The supported target contract is explicitly 64-bit; Rust MSRV remains 1.77.

### Fixed

- Hardened file-size conversion and scanner/partition arithmetic against
  overflow and unsupported 32-bit ABI layouts.
- `ChunkCursor::size_hint()` and `PatternChunkCursor::size_hint()`: lower bound corrected
  from bogus `(remaining / step) + 1` to `1` (there is always at least one more chunk
  when the cursor is not exhausted). The old lower bound could exceed the true remaining
  count, violating the `Iterator` contract.
- `ChunkCursor::is_empty()` and `PatternChunkCursor::is_empty()`: implementation changed
  from `self.data.is_empty()` (which reports whether *underlying* data is empty) to
  `self.position >= self.data.len()` (which reports whether the cursor is exhausted),
  matching the documented contract.

### Changed

- Cursor benchmark methodology hardened with `black_box`, 7-sample p50/p10/p90

## [0.2.0] — 2026-08-08

### Added

- Record-aligned partition planning for N-way parallel consumers
  - `scanner::find_partition_boundaries(data, num_partitions, delimiter)` — Rust primitive
  - `mmap_engine_partition_records(handle, requested, delimiter)` — C ABI function
  - `ChunkLayout::Partitioned(Vec<(usize, usize)>)` — new layout variant
  - `CAP_RECORD_PARTITIONING` (bit 4) — capability discovery
  - ABI version bumped to `0x0001_0002` (v1.2, additive)
  - Absolute-target algorithm: each boundary computed independently from
    `floor(file_len * i / N)`, preventing cumulative size drift
  - Record integrity guaranteed: no record split, boundaries collapsed on
    giant records instead of creating empty partitions
  - O(N) metadata, bounded scanning (≤ file_len total)
  - Mode switching: delimited ⇔ fixed ⇔ partitioned via re-plan
- Fixed-size chunking via arithmetic layout (O(1) metadata, zero scan cost)
  - `mmap_engine_scan_fixed(handle, chunk_size)` — C ABI function
  - `scanner::fixed_chunk_count(file_len, chunk_size)` — pure arithmetic helper
  - `scanner::fixed_chunk_bounds(file_len, chunk_size, index)` — pure arithmetic helper
  - `CAP_FIXED_SIZE_CHUNKING` (bit 3) — capability discovery
  - ABI version bumped to `0x0001_0001` (v1.1, additive)
- `ChunkLayout` enum replacing `Engine.chunks: Vec<(usize,usize)>`
  - `Empty`, `Delimited(Vec<(usize,usize)>)`, `Fixed { chunk_size, chunk_count }`, `Partitioned(Vec<(usize, usize)>)`
  - Mode switching: any scan/plan replaces previous layout (delimited⇔fixed⇔partitioned)

### Fixed

- `tests/performance.rs` measurement defects:
  - `search_bytes` now counts actual examined bytes (early-exit semantics) instead
    of available remainder span (was distorted ~500,000x)
  - Removed duplicated `find_byte_swar` benchmark copy; uses production fn
  - Replaced duplicated `find_chunk_boundaries_swar` with genuine scalar baseline
  - Renamed "End-to-End" label to "Scanner Benchmark (in-memory)"
  - Added sample count and build mode to benchmark output
  - Renamed `total_ns` → `avg_ns` for clarity

### Changed

- `find_byte_swar` in `src/scanner.rs` changed from `fn` to `pub(crate)` (internal only; byte-search benchmarks relocated to scanner test module for internal access)
- `tests/performance.rs` scanner comparison now measures production SWAR vs
  genuine scalar baseline (previously SWAR vs SWAR)
- `abi/v1.symbols` now covers v1.x (added `mmap_engine_scan_fixed`, `mmap_engine_partition_records`)
- `mmap_chunker.h` updated with v1.2 version, new capability bits, new functions
- `examples/c_consumer.c` extended with 8 record partition test scenarios

## [0.1.0] — 2026-08-08

### Added

- Initial release with POSIX `mmap` and Windows `CreateFileMappingW` support
- Newline-delimited chunking via `scanner::find_chunk_boundaries`
- C ABI with 8 public functions:
  - `mmap_engine_open` — open and memory-map a file
  - `mmap_engine_scan_chunks` — scan with newline delimiter (v1.0 compat)
  - `mmap_engine_scan_chunks_ex` — scan with configurable single-byte delimiter
  - `mmap_engine_get_chunk` — retrieve zero-copy chunk view by index
  - `mmap_engine_free` — release all resources
  - `mmap_engine_abi_version` — runtime ABI version discovery
  - `mmap_engine_capabilities` — feature detection bitmask
  - `mmap_engine_last_error` — thread-local diagnostic error string
- Zero-copy `CChunkView` struct (`#[repr(C)]`, 16 bytes on 64-bit)
- Opaque `CEngineHandle` (ZST marker pattern)
- `MADV_SEQUENTIAL` advisory hint on POSIX
- Windows UTF-8 to UTF-16 path conversion
- Zero-size file handling (valid handle, 0 chunks)
- Panic containment via `std::panic::catch_unwind` at all FFI boundaries
- `mmap_engine_free` aborts on panic (void function, no error return)
- `Send` + `Sync` on `MmapFile` (read-only mapping, safe to share)
- Static library (`staticlib`) and dynamic library (`cdylib`) targets
- `Cargo.toml`: dual-licensed under MIT OR Apache-2.0
- C header (`mmap_chunker.h`) with full API documentation, threading contract,
  file mutation contract, and ABI stability notes
- 47 Rust tests (45 unit + 2 integration) including property tests for
  concatenation, gap-freedom, determinism, monotonic offsets, and alternative
  delimiters
- Benchmarks comparing mmap vs `std::fs::read` path
- Python ctypes consumer example (`native_io/`)
