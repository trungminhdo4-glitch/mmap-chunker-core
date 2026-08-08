# mmap-chunker-core

Zero-dependency data chunking engine with native memory-mapped I/O and a stable C ABI.

## Why

Splitting large files into record-delimited chunks is a common task in data
pipelines, log processing, and ETL workloads. Most solutions either copy data
unnecessarily or pull in heavy dependencies. This library provides:

- **Zero-copy** chunk views backed by OS-level memory mapping
- **Zero runtime dependencies** — pure Rust with direct syscall FFI
- **Language-agnostic C ABI** — usable from C, Python, Go, C#, and any language with FFI
- **Delimiter-aware** chunking — boundaries placed at record boundaries, not arbitrary offsets

## Features

- Platform support: Linux, Windows, macOS
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
for (size_t i = 0; i < count; i++) {
    CChunkView view;
    mmap_engine_get_chunk(h, i, &view);
    fwrite(view.data, 1, view.len, stdout);
}

mmap_engine_free(h);
```

## Rust Usage

```rust
use mmap_chunker_core::scanner;

let data = std::fs::read("records.csv")?;
let chunks = scanner::find_chunk_boundaries(&data, 65536, b',');
for (start, end) in &chunks {
    let chunk = &data[*start..*end];
    // process chunk
}
```

## Dynamic Library (cdylib)

```python
# Python with ctypes
import ctypes
lib = ctypes.CDLL("./mmap_chunker_core.dll")
lib.mmap_engine_abi_version.restype = ctypes.c_uint32
assert lib.mmap_engine_abi_version() == 0x00010000
```

See `native_io/` for a full Python ctypes example with protocol-based provider abstraction.

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
cargo test --test benchmark -- --ignored --nocapture
```

Runs on 1 MB, 16 MB, and 64 MB files with 64 KB, 256 KB, and 1 MB chunk sizes.
Outputs wall-clock time and throughput for mmap vs `std::fs::read` path.

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

46 tests including property tests, ABI layout verification, and C ABI integration.

## Limitations

- Full-file mapping only (no windowed mmap). Very large files may exhaust address space.
- Single-byte delimiter only. Multi-byte or regex delimiters not supported.
- No copy-on-write or mutable access. Read-only mapping.
- No lazy/streaming chunk iteration. Chunks are computed eagerly before first access.

## Roadmap

- Multi-byte delimiter support (`\r\n`, custom record separators)
- Lazy chunk cursor for streaming consumers
- Fixed-size chunking mode (delimiter-independent)
- External C consumer validation test

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
