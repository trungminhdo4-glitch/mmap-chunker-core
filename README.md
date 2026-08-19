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

Verified prebuilt native libraries are published on [GitHub Releases](https://github.com/trungminhdo4-glitch/mmap-chunker-core/releases) for tagged releases that carry native assets. Each platform archive contains the C header, dynamic library, static library, and licenses.

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

### Linux x86_64 compatibility

The `x86_64-unknown-linux-gnu` release workflow declares a `GLIBC_2.17`
symbol ceiling and checks it from the final extracted archive. Each candidate
also runs that extracted `.so` with the maintained C conformance consumer in a
digest-pinned manylinux2014 (GNU libc 2.17) environment. The runtime gate checks
loading, ABI/capability discovery, UTF-8 paths, deterministic record partitions,
exact reconstruction, the N=0 error contract, and clean handle release. The
same archive also has C, Python ctypes, Go/cgo, and C# conformance coverage on
the modern Linux runner. This is a bounded release contract, not a claim that
every system with glibc at or above that version has been tested.

The extracted archive layout is `staging/include/mmap_chunker.h` and
`staging/lib/` for the shared and static libraries. For a shared-library
consumer, use the platform loader's normal search configuration, such as an
RPATH/RUNPATH or `LD_LIBRARY_PATH`; the library itself does not embed a
SONAME, RPATH, or RUNPATH. The static file is a static library archive, not a
fully static executable. A typical Linux link supplies the system libraries,
for example `cc -I staging/include -L staging/lib -o app app.c
-lmmap_chunker_core -lpthread -ldl`.

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

## Command-line partitioning

Install the CLI with Cargo:

```sh
cargo install mmap-chunker-core
mmap-chunker partition records.jsonl --parts 8
# Ask an independently launched worker for only its zero-based range.
mmap-chunker partition records.jsonl --parts 8 --worker 3
# Partition binary records on the NUL byte.
mmap-chunker partition records.bin --parts 8 --delimiter-byte 0
```

`partition` writes one tab-separated numeric range per line; stdout has no header:

```text
0	0	12739120	12739120
1	12739120	25478291	12739171
```

Offsets are bytes. Starts are inclusive and ends are exclusive, so
`end_exclusive - start == length`. The default record delimiter remains newline
byte `0x0A`. `--delimiter-byte B` accepts one decimal raw byte in the range
`0..255`, including arbitrary binary delimiters such as NUL and `0xFF`. Ranges
are deterministic, contiguous, and record-aligned; the actual range count can
be lower than requested when giant records span multiple ideal partition
positions. This is framing and planning only, not CSV/JSON parsing; multi-byte
partition delimiters are not supported. The input file must remain immutable
while it is mapped.

With `--worker K`, `K` must be less than `--parts` and the CLI emits only the
zero-based range at index `K`. This lets independently launched workers request
their own byte range. If record-aligned boundaries collapse and no actual range
exists at a valid index, the command succeeds with no output.

### Ordered multi-file logical dataset and worker reference

The CLI also contains a deliberately small composition proof for an ordered set
of independent local files:

```sh
mmap-chunker partition-files --parts 8 file-a.jsonl file-b.jsonl file-c.jsonl
# The delimiter option is the same raw single-byte framing contract:
mmap-chunker partition-files --parts 8 --delimiter-byte 0 file-a.bin file-b.bin
```

`partition-files` accepts only the explicitly ordered file paths shown on the
command line. Each input is mapped independently and remains a separate source;
the CLI does not concatenate files, copy them into a temporary file, or create a
virtual contiguous address space. Duplicate paths are valid and are treated as
distinct sources in the order supplied. Directory traversal, globbing, stdin,
manifests, and watching are not part of this proof.

Its headerless TSV output has exactly five fields per row:

```text
worker_index<TAB>source_index<TAB>start<TAB>end_exclusive<TAB>length
```

`source_index` is the zero-based input argument index. `start` is inclusive and
`end_exclusive` is exclusive local byte offset within that source, and
`end_exclusive - start == length`. Rows are ordered by compact zero-based
`worker_index`, then by ascending `source_index`; a worker can therefore emit
multiple source ranges. Empty sources produce no rows. A dataset containing only
empty sources succeeds with empty stdout. Omitting all source paths is an error.

The planner computes ideal worker targets over the sum of all source lengths. A
target at a file boundary is kept because file boundaries are valid logical
segment boundaries. A target inside a source is projected forward to the next
single-byte delimiter boundary, or to that source's EOF when no delimiter
remains. Records never cross a source boundary or a worker boundary. The actual
worker count can be lower than `--parts` when multiple ideal targets fall inside
one record; `worker_index` is then compacted to the workers that received bytes.
The result is deterministic. This is planning/framing only: record alignment can
dominate the ideal byte targets, so no universal balance guarantee is implied.

For a small real consumer, see
[`examples/jsonl_multi_file_workers.py`](examples/jsonl_multi_file_workers.py):

```sh
python examples/jsonl_multi_file_workers.py --parts 4 \
  shard-z.jsonl shard-a.jsonl shard-z.jsonl
```

```text
ordered JSONL shards
        ↓
mmap-chunker partition-files
        ↓
group five-column plan by worker
        ↓
spawn independent workers
        ↓
seek/read assigned source-local ranges and parse JSON
```

This is a reference integration, not a universal speedup claim. The planner
owns range selection; Python owns worker execution and JSON parsing. Process
startup can dominate small workloads, and one worker may receive multiple
ranges from multiple sources. `source_index` is resolved through the original
ordered path list, including duplicate paths. The larger
[`jsonl_multi_file_worker_proof.py`](examples/jsonl_multi_file_worker_proof.py)
keeps the independent oracle and bounded pathological-case matrix.

### Installing the standalone CLI

Rust users can install the CLI from source with Cargo:

```sh
cargo install mmap-chunker-core
```

Standalone users can download the matching
`mmap-chunker-<version>-<target>.tar.gz` (or Windows `.zip`) archive from
[GitHub Releases](https://github.com/trungminhdo4-glitch/mmap-chunker-core/releases),
verify its `.sha256` sidecar, extract it, and run `mmap-chunker`. The archive
contains only the executable and the MIT/Apache license files; it does not
include the native-library package.

Once a crate release carrying this metadata is published, users with
`cargo-binstall` can run `cargo binstall mmap-chunker-core`; it will use the
prebuilt CLI when an archive exists for the target.

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
| Single-process reference | — | — | — | 483.2 ms |
| 1 worker | 0.3 ms | 9.5 ms | 425.5 ms | 432.0 ms |
| 2 workers | 0.3 ms | 11.0 ms | 289.7 ms | 301.0 ms |
| 4 workers | 0.4 ms | 14.2 ms | 215.0 ms | 234.7 ms |

All modes processed the same 100,000 records and 11,518,914 bytes and produced
the same value sum of 49,843,048,239. These figures are an
adoption/correctness proof, not a universal performance claim. Python process
startup and JSON decoding dominate this small local workload, and timings vary
with machine state and workload.

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
