# mmap-chunker-core

[![CI](https://github.com/trungminhdo4-glitch/mmap-chunker-core/actions/workflows/ci.yml/badge.svg)](https://github.com/trungminhdo4-glitch/mmap-chunker-core/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/mmap-chunker-core.svg)](https://crates.io/crates/mmap-chunker-core)
[![docs.rs](https://docs.rs/mmap-chunker-core/badge.svg)](https://docs.rs/mmap-chunker-core)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](LICENSE-MIT)

Record-aligned byte-range planning for large immutable local files, with
zero-copy framing and a stable C ABI.

The core is a zero-dependency Rust library that maps a file, finds framing
boundaries, and returns views or contiguous ranges for independent consumers.

## Why

When a large JSONL/NDJSON or newline-delimited log file must be processed by N
independent local workers, the awkward part is choosing balanced byte ranges
without splitting a record. This library provides the small planning and
framing primitive underneath that worker pipeline:

- **Zero-copy** chunk views backed by OS-level memory mapping
- **Zero runtime dependencies** — pure Rust with direct syscall FFI
- **Language-agnostic C ABI** — usable from C, Python, Go, C#, and any language with FFI
- **Record-aligned N-way partitioning** — deterministic, contiguous ranges for independent workers
- **Three planning modes**: delimiter framing, fixed-size chunking, and record-aligned partitioning

## Framing, not parsing

The engine is byte- and delimiter-aware; it does not parse a file format. A
comma delimiter means raw comma framing, not CSV semantics. Quoted commas,
escaped delimiters, and multiline quoted CSV records are not interpreted.
Likewise, JSON grammar, protobuf framing, compression, and application-level
validation remain the consumer's responsibility.

Use newline framing for one-record-per-line JSONL/NDJSON and ordinary logs. For
CSV or other structured formats, pair the range planner with a format-aware
parser and only use a delimiter when its record-boundary rules are compatible
with the file.

## Non-goals

This is not a CSV or JSON parser, ETL/dataframe engine, distributed scheduler,
RAG/text chunker, content-defined chunker, or general-purpose byte-search
package. It is a small local-file framing and partitioning primitive; parsing,
validation, worker scheduling, and cross-machine coordination remain with the
consumer.

## Features

- Targets Windows and POSIX platforms (Linux, macOS)
- Windows and Linux are validated in CI; macOS validation now included
- POSIX `mmap` / Windows `CreateFileMappingW`
- Configurable raw single-byte delimiter (newline, comma, tab, pipe, NUL, etc.)
- Multi-byte delimiter support (e.g., `b"\r\n"` for CRLF, `b"\r\n\r\n"` for HTTP-style) — Rust and C ABI
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
// For CRLF or another binary pattern, use pointer + length (ABI v1.3):
// const uint8_t delimiter[] = {'\r', '\n'};
// count = mmap_engine_scan_chunks_pattern(h, 64 * 1024, delimiter, 2);
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

## Prebuilt Libraries (C / Python / Go / FFI)

Verified prebuilt native libraries are published on [GitHub Releases](https://github.com/trungminhdo4-glitch/mmap-chunker-core/releases) for releases that carry native assets (currently v0.2.1). Each platform archive contains the C header, dynamic library, static library, and licenses.

| Platform | Archive | Contents |
|----------|---------|----------|
| Linux x86_64 | `mmap-chunker-core-{ver}-x86_64-unknown-linux-gnu.tar.gz` | `.so`, `.a` |
| Linux aarch64 | `mmap-chunker-core-{ver}-aarch64-unknown-linux-gnu.tar.gz` | `.so`, `.a` |
| macOS x86_64 | `mmap-chunker-core-{ver}-x86_64-apple-darwin.tar.gz` | `.dylib`, `.a` |
| macOS arm64 | `mmap-chunker-core-{ver}-aarch64-apple-darwin.tar.gz` | `.dylib`, `.a` |
| Windows x86_64 | `mmap-chunker-core-{ver}-x86_64-pc-windows-msvc.zip` | `.dll`, `.dll.lib`, `.lib` |

```python
# Python with ctypes (download archive, extract, load)
import ctypes
lib = ctypes.CDLL("./libmmap_chunker_core.so")  # or .dll / .dylib
lib.mmap_engine_abi_version.restype = ctypes.c_uint32
assert lib.mmap_engine_abi_version() == 0x00010003
```

```c
// C: compile against extracted archive
// cc -I staging/include/ -L staging/lib/ -lmmap_chunker_core your_program.c
#include "mmap_chunker.h"
uint32_t ver = mmap_engine_abi_version();
```

See `mmap_chunker.h` for the complete C API reference with threading and safety contracts.

## Example: parallel JSONL worker ranges

[`examples/jsonl_multiprocessing_proof.py`](examples/jsonl_multiprocessing_proof.py)
is a dependency-free reference integration, not a Python binding. It loads the
native library with stdlib `ctypes`, calls
`mmap_engine_partition_records()`, reads each partition length through the
existing `CChunkView`, and reconstructs `(offset, length)` ranges by cumulative
addition. Each spawned worker opens the original file independently and parses
only its assigned range.

Run it after `cargo build --release`:

```sh
python examples/jsonl_multiprocessing_proof.py \
  --records 100000 --payload-bytes 64 --workers 1,2,4 --repeats 3
```

The proof checks exact record count, numeric sum, byte coverage, and deterministic
results against a single-process reference. It reports partition planning,
worker startup, processing, and end-to-end wall time separately. Process startup
and application parsing can dominate small workloads, so the example makes no
universal multiprocessing speed claim.

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

The cursor benchmark reports time-to-first-chunk and full traversal for
synthetic JSONL/log-like byte buffers. It uses a release build, seven samples
per case, and prints p10/p50/p90 in nanoseconds. These are API-shape
measurements—not end-to-end file-processing throughput—and are expected to
vary by CPU, toolchain, and workload. The I/O benchmark is separately labeled
as warm/cached mmap versus `fs::read`; run the commands above for current
machine-specific output.

One bounded Windows proof run (Windows 11, x86_64, 12 logical CPUs, Python
3.12.6, release DLL, 3-sample medians, 100,000 generated JSONL records,
11,518,914 bytes) produced:

| Mode | Median planning | Median worker startup | Median processing | Median end-to-end |
|------|----------------:|----------------------:|------------------:|------------------:|
| Single-process reference | — | — | — | 1,220.3 ms |
| 1 worker | 1,868.1 ms | 6.1 ms | 421.5 ms | 2,314.2 ms |
| 2 workers | 0.2 ms | 11.3 ms | 283.6 ms | 295.1 ms |
| 4 workers | 0.2 ms | 24.0 ms | 278.2 ms | 302.4 ms |

All modes processed the same 100,000 records and 11,518,914 bytes and produced
the same value sum of 49,843,048,239. The 1-worker planning measurement
included a one-time cold-start outlier on this run; these figures are an
adoption/correctness proof, not a universal performance claim. Python process
startup and JSON decoding dominate this small local workload.

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

The suite covers delimiter semantics, cursor equivalence, fixed-size chunking,
partitioning, C ABI behavior, and edge cases.

Companion test suites:
- External C ABI consumer scenarios covering ABI discovery, errors, delimiter/pattern/fixed/partition modes, layout, and coverage (CI-validated on Linux and macOS)
- Deterministic Python ctypes semantic parity in `tests/python_parity.py` (CI-validated on Linux)

## Limitations

- Full-file mapping only (no windowed mmap). Very large files may exhaust address space.
- No copy-on-write or mutable access. Read-only mapping.
- No regex delimiters. Multi-byte delimiters supported (e.g., `b"\r\n"`, `b"\r\n\r\n"`).

## Roadmap

- More real consumer integrations for record-aligned local worker pipelines
- Benchmark-backed search backend decisions; no custom SIMD promise without a measured win

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
