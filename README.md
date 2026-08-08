# mmap-chunker-core

[![CI](https://github.com/trungminhdo4-glitch/mmap-chunker-core/actions/workflows/ci.yml/badge.svg)](https://github.com/trungminhdo4-glitch/mmap-chunker-core/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](LICENSE-MIT)

Zero-dependency data chunking engine with native memory-mapped I/O and a stable C ABI.

## Why

Splitting large files into record-delimited chunks is a common task in data
pipelines, log processing, and ETL workloads. Most solutions either copy data
unnecessarily or pull in heavy dependencies. This library provides:

- **Zero-copy** chunk views backed by OS-level memory mapping
- **Zero runtime dependencies** — pure Rust with direct syscall FFI
- **Language-agnostic C ABI** — usable from C, Python, Go, C#, and any language with FFI
- **Three planning modes**: delimiter-aware chunking, fixed-size chunking, and record-aligned N-way partitioning

## Features

- Targets Windows and POSIX platforms (Linux, macOS)
- Windows and Linux are validated in CI; macOS validation now included
- POSIX `mmap` / Windows `CreateFileMappingW`
- Configurable single-byte delimiter (newline, comma, tab, pipe, NUL, etc.)
- Zero-copy `CChunkView` — chunk pointers reference the mapped file directly
- `MADV_SEQUENTIAL` hint for sequential scan throughput
- Panic containment at all FFI boundaries
- Thread-safe chunk retrieval after scan
- Immutable input contract with documented file-mutation semantics

## Architecture

```
┌──────────┐    C ABI     ┌──────────────────┐
│ C / Go / │◄────────────►│ mmap-chunker-core │
│ Python   │              │                  │
│ C#       │              │  open  ─► mmap   │
│          │              │  scan  ─► chunks │
│          │              │  get   ─► view   │
│          │              │  free            │
└──────────┘              └──────────────────┘
```

## C API

```c
#include "mmap_chunker.h"

// Discover library version and capabilities
uint32_t ver = mmap_engine_abi_version();
uint32_t caps = mmap_engine_capabilities();

// Open and scan a file
CEngineHandle *h = mmap_engine_open("/data/records.jsonl");
if (!h) {
    fprintf(stderr, "Error: %s\n", mmap_engine_last_error());
    return 1;
}

size_t count = mmap_engine_scan_chunks_ex(h, 64 * 1024, '\n');
// or: mmap_engine_scan_fixed(h, 4096)              — fixed-size mode
// or: mmap_engine_partition_records(h, 4, '\n')   — N-way partition planning
for (size_t i = 0; i < count; i++) {
    CChunkView view;
    mmap_engine_get_chunk(h, i, &view);
    fwrite(view.data, 1, view.len, stdout);
}

mmap_engine_free(h);
```

## Rust Usage

```rust
use mmap_chunker_core::MmapChunker;

// ── Indexed (random access) ─────────────────────────────────
// Pre-computes chunk boundaries → O(1) random access by index.
// O(number_of_chunks) heap metadata (~16 bytes per chunk).

let mut file = unsafe { MmapChunker::open("records.jsonl")? };
let count = file.scan_delimited(65536, b'\n');
let third = file.get_chunk(2);
// Iterate all
for i in 0..count {
    if let Some(chunk) = file.get_chunk(i) {
        let _data: &[u8] = chunk;
    }
}

// ── Streaming (low memory) ─────────────────────────────────
// Yields chunks sequentially without building a boundary Vec.
// O(1) state (~40 bytes on 64-bit) regardless of file size.
// Ideal for single-pass consumers, pipelines, and large files.

let file = unsafe { MmapChunker::open("records.jsonl")? };
for chunk in file.delimited_cursor(65536, b'\n') {
    let _data: &[u8] = chunk;
}

// ── Other scan modes ────────────────────────────────────────

let mut file = unsafe { MmapChunker::open("data.bin")? };

// Fixed-size chunks (no delimiter)
let n = file.scan_fixed(4096);
let block = file.get_chunk(0);

// Record-aligned N-way partitioning
let parts = file.partition_records(4, b'\n');
for i in 0..parts {
    let partition = file.get_chunk(i).unwrap();
}

// Multi-byte delimiters (CRLF, HTTP-style, custom separators)
let mut file = unsafe { MmapChunker::open("data.txt")? };
let n = file.scan_delimited_pattern(65536, b"\r\n");
let chunk = file.get_chunk(0);

// Lazy cursor with multi-byte delimiter
let file = unsafe { MmapChunker::open("data.txt")? };
for chunk in file.delimited_cursor_pattern(65536, b"\r\n\r\n") {
    let _data: &[u8] = chunk;
}
```

## Scanner primitives (standalone, no mmap)

```rust
use mmap_chunker_core::scanner;

let data = b"aaa\nbbb\nccc\nddd\n";

// 1. Eager delimiter-aware chunking — returns Vec<(usize, usize)>
let chunks = scanner::find_chunk_boundaries(data, 4, b'\n');

// 2. Lazy delimiter cursor — yields &[u8] slices on demand
let slices: Vec<&[u8]> = scanner::ChunkCursor::new(data, 4, b'\n').collect();

// 3. Multi-byte delimiter scanner — e.g., CRLF, HTTP-style separators
let chunks = scanner::find_chunk_boundaries_pattern(data, 4, b"\r\n");

// 4. Lazy multi-byte cursor
let slices: Vec<&[u8]> = scanner::PatternChunkCursor::new(data, 4, b"\r\n\r\n").collect();

// 5. Fixed-size chunking — O(1) arithmetic layout, zero scan cost
let count = scanner::fixed_chunk_count(data.len(), 4096);
let bounds = scanner::fixed_chunk_bounds(data.len(), 4096, 0);

// 6. Record-aligned N-way partitioning — for parallel consumers
let partitions = scanner::find_partition_boundaries(data, 4, b'\n');
```

## Dynamic Library (cdylib)

```python
# Python with ctypes
import ctypes
lib = ctypes.CDLL("./mmap_chunker_core.dll")
lib.mmap_engine_abi_version.restype = ctypes.c_uint32
assert lib.mmap_engine_abi_version() == 0x00010002
```

See the `mmap_chunker.h` header for the complete C API reference with threading and safety contracts.

## Safety Contract

- **Handle owns all resources**: mmap, chunk metadata. Freed with `mmap_engine_free`.
- **Chunk views borrow from handle**: valid until `mmap_engine_free`. Use-after-free is undefined.
- **Immutable input**: The file must not be truncated or overwritten while the handle is live.
- **Panic isolation**: All FFI boundaries catch panics. `mmap_engine_free` aborts on panic (no return value for error).
- **Threading**: Single-threaded open/scan/free. Multi-threaded chunk retrieval after scan.

## File Mutation Contract

The engine provides a read-only view of the file at mapping time. If another
process truncates or overwrites the file:
- **POSIX**: May deliver `SIGBUS` or return zero-filled pages
- **Windows**: Mapped view may become invalid (access violation)

**Recommendation**: Treat the input file as immutable for the handle lifetime.

## Benchmarks

```sh
# I/O benchmark (mmap vs fs::read)
cargo test --test benchmark -- --ignored --nocapture

# Cursor vs eager time-to-first-chunk + full traversal
cargo test --release scanner::tests::bench_cursor_vs_eager -- --ignored --nocapture
```

Time-to-first-chunk (TFC) advantage with lazy cursor on JSONL/log data (64 KiB chunks, release build, 7-sample p50):

| File Size | Eager TFC  | Lazy TFC | Speedup | Chunks |
|-----------|-----------|----------|---------|--------|
| 100 KB    | 139 ns    | 8.5 ns   | 16x     | 2      |
| 1 MB      | 554 ns    | 10 ns    | 55x     | 16     |
| 10 MB     | 2,780 ns  | 10 ns    | 278x    | 153    |

Full traversal converges as file size grows (both do equivalent scan work).
Lazy is 2.2x faster at 1 MB and 1.3x faster at 10 MB (Vec allocation overhead).
I/O benchmark runs on 1 MB–64 MB files with 64 KB–1 MB chunk sizes.

## Build

```sh
cargo build --release
```

Outputs:
- `target/release/mmap_chunker_core.dll` (Windows)
- `target/release/libmmap_chunker_core.so` (Linux/macOS)
- `target/release/libmmap_chunker_core.a` (static library)

## Tests

```sh
cargo fmt --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

181 tests (179 unit + 2 integration) including property tests for concatenation,
gap-freedom, determinism, monotonic offsets, delimiter variants, fixed-size chunking,
and record-aligned partition planning.

Companion test suites:
- 30 external C ABI assertions via `examples/c_consumer.c` (CI-validated on Linux and macOS)
- 53 Python ctypes integration tests (local, companion module)

## Limitations

- Full-file mapping only (no windowed mmap). Very large files may exhaust address space.
- No copy-on-write or mutable access. Read-only mapping.
- No regex delimiters. Multi-byte delimiters supported (e.g., `b"\r\n"`, `b"\r\n\r\n"`).

## Roadmap

- SIMD-accelerated byte search (runtime dispatch)

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
