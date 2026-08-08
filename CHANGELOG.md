# Changelog

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
- 33 Rust unit + integration tests (pre-evolution)
- 46 tests including property tests for concatenation, gap-freedom, determinism,
  monotonic offsets, and alternative delimiters
- Benchmarks comparing mmap vs `std::fs::read` path
- Python ctypes consumer example (`native_io/`)
