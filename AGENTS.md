# AGENTS.md — mmap-chunker-core

## Project

Zero-dependency Rust library for memory-mapped file chunking via a stable C ABI.
Language-agnostic, harness-independent, standalone open-source product.

## Start Commands

```sh
cargo fmt --check        # Format check
cargo check              # Fast compile check
cargo clippy --all-targets -- -D warnings   # Lint
cargo test               # 179 unit + 2 integration tests (181 total)
cargo build --release    # Produces staticlib + cdylib
```

Benchmarks:
```sh
cargo test --test benchmark -- --ignored --nocapture           # I/O mmap vs fs::read
cargo test --release scanner::tests::bench_cursor_vs_eager -- --ignored --nocapture  # Cursor TFC
```

Python tests (requires release build first):
```sh
python -m pytest tests/test_native_io.py -v
```

## Architecture

```
src/
  lib.rs      — module declarations + public re-exports
  mmap.rs     — MmapFile: platform-specific mmap (Unix/Win), Send+Sync
  scanner.rs  — find_chunk_boundaries (delimiter), ChunkCursor (lazy iterator),
                 PatternChunkCursor (multi-byte delimiter cursor),
                 find_byte_swar (SWAR, pub(crate)), fixed_chunk_count/bounds,
                 find_partition_boundaries (N-way)
  ffi.rs      — C ABI: 10 public functions, ChunkLayout enum, panic containment

tests/
  c_abi_test.rs   — Integration test: C ABI via extern "C" (Rust calling Rust)
  benchmark.rs    — Performance: mmap vs fs::read

native_io/        — Python ctypes consumer (standalone, no harness dependency)
mmap_chunker.h    — Public C header with full API docs
```

## Key Invariants

- **0 runtime dependencies** — no crates in `[dependencies]`
- **Read-only mmap** — `MmapFile` is immutable, `Send + Sync`
- **Panic containment** — all FFI boundaries use `catch_unwind`
- **C ABI stability** — additive changes only, ABI version via `mmap_engine_abi_version()`
- **Immutable input contract** — file must not mutate while handle lives
- **Threading**: open/scan/free = single-thread, get_chunk = multi-thread after scan
- **No harness imports** — this library has no dependency on any agent harness

## Public C ABI (10 functions)

| Function                          | Purpose                              |
|-----------------------------------|--------------------------------------|
| `mmap_engine_abi_version()`       | Returns `(major << 16) \| minor`     |
| `mmap_engine_capabilities()`      | Feature detection bitmask            |
| `mmap_engine_last_error()`        | Thread-local error diagnostics       |
| `mmap_engine_open(path)`          | Open + mmap file                     |
| `mmap_engine_scan_chunks(h, sz)`  | Scan with newline delimiter (v1.0)   |
| `mmap_engine_scan_chunks_ex(h,sz,delim)` | Scan with configurable delimiter |
| `mmap_engine_scan_fixed(h, sz)`   | Fixed-size arithmetic chunking (v1.1)|
| `mmap_engine_partition_records(h, n, d)` | Record-aligned N-way partition (v1.2)|
| `mmap_engine_get_chunk(h, i, out)`| Zero-copy chunk by index (returns 0/-1) |
| `mmap_engine_free(h)`             | Release all resources (abort on panic)|

## Gotchas

- Windows GNU toolchain (`x86_64-pc-windows-gnu`) by default — MSVC also installed
- Redundant `unsafe` blocks inside outer `unsafe` blocks trigger clippy `unused_unsafe`
- `extern "C" fn` that are inherently safe (no pointer access) should NOT be `unsafe`
- Cargo.lock is gitignored (library, not application) but exists in-tree from early commit
- Python native_io module requires `cargo build --release` before tests
