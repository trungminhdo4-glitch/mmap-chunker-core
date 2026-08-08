# Changelog

## [Unreleased]

### Added

- Fixed-size chunking via arithmetic layout (O(1) metadata, zero scan cost)
  - `mmap_engine_scan_fixed(handle, chunk_size)` — C ABI function
  - `scanner::fixed_chunk_count(file_len, chunk_size)` — pure arithmetic helper
  - `scanner::fixed_chunk_bounds(file_len, chunk_size, index)` — pure arithmetic helper
  - `CAP_FIXED_SIZE_CHUNKING` (bit 3) — capability discovery
  - ABI version bumped to `0x0001_0001` (v1.1, additive)
- `ChunkLayout` enum replacing `Engine.chunks: Vec<(usize,usize)>`
  - `Empty`, `Delimited(Vec<(usize,usize)>)`, `Fixed { chunk_size, chunk_count }`
  - Mode switching: any scan replaces previous layout (delimited⇔fixed)

### Fixed

- `tests/performance.rs` measurement defects:
  - `search_bytes` now counts actual examined bytes (early-exit semantics) instead
    of available remainder span (was distorted ~500,000x)
  - Removed duplicated `find_byte_swar` benchmark copy; uses production `pub` fn
  - Replaced duplicated `find_chunk_boundaries_swar` with genuine scalar baseline
  - Renamed "End-to-End" label to "Scanner Benchmark (in-memory)"
  - Added sample count and build mode to benchmark output
  - Renamed `total_ns` → `avg_ns` for clarity

### Changed

- `find_byte_swar` in `src/scanner.rs` changed from `fn` to `pub fn` (benchmark access)
- `tests/performance.rs` scanner comparison now measures production SWAR vs
  genuine scalar baseline (previously SWAR vs SWAR)
- `abi/v1.symbols` now covers v1.x (added `mmap_engine_scan_fixed`)
- `mmap_chunker.h` updated with v1.1 version, new capability bit, new function

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
