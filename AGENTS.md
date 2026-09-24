# AGENTS.md — mmap-chunker-core

## Project

Zero-dependency Rust library for memory-mapped file chunking via a stable C ABI.
Language-agnostic, harness-independent, standalone open-source product.

## Start Commands

```sh
cargo fmt --check        # Format check
cargo check              # Fast compile check
cargo clippy --all-targets -- -D warnings   # Lint
cargo test               # run the full Rust test suite
cargo build --release    # Produces staticlib + cdylib
```

Benchmarks:
```sh
cargo test --test benchmark -- --ignored --nocapture           # I/O mmap vs fs::read
cargo test --release --test benchmark benchmark_source_modes -- --ignored --nocapture  # mmap/windowed/pread planner
cargo test --release scanner::tests::bench_cursor_vs_eager -- --ignored --nocapture  # Cursor TFC
cargo test --release --test competitive_bench -- --ignored --nocapture  # Full competitive suite (Lanes A-F)
```

Python tests (requires release build first):
```sh
python -m pytest tests -q                 # test_native_io.py + test_plan_manifest.py + test_record_index.py
python tests/python_parity.py             # C-ABI parity incl. planner source modes + framed planning
```

**Toolchain note:** the rustup default `1.95-x86_64-pc-windows-gnu` currently
has a broken `cargo` component. Use `cargo +1.94-x86_64-pc-windows-gnu ...`
(or the MSVC toolchain) for local builds.

**Heavy runtime goes to Netcup** (owner directive): local runs are limited to
fast edit/check loops. Heavy suites, MSRV and DataFusion builds run in
ephemeral Docker containers on the Netcup host via the project tool:

```sh
python tools/remote_verify.py --image rust:1.94 -- sh -c "cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test"
python tools/remote_verify.py --image rust:1.77 -- cargo check --all-targets
python tools/remote_verify.py --dir integrations/datafusion -- cargo test
```

The tool packages the worktree, ships it with `scp`, runs the command in
`rust:1.94`/`rust:1.77` via `sudo -n docker run --rm`, streams output, and
writes `target/remote_verify_receipt.json`. Requirements: SSH key access to
the default host and `sudo -n docker` there; no host installs. The repo is
never left behind (sudo cleanup trap).

## Architecture

```
src/
  lib.rs      — module declarations + public re-exports
  mmap.rs     — MmapFile (full mmap) + WindowedMmapFile (bounded 64 KiB-aligned moving window), Send+Sync
  scanner.rs  — find_chunk_boundaries (delimiter), ChunkCursor (lazy iterator),
                 PatternChunkCursor (multi-byte delimiter cursor),
                 find_byte_swar (SWAR, pub(crate)), fixed_chunk_count/bounds,
                 find_partition_boundaries (N-way), find_partition_boundaries_pattern,
                 ranges_from_boundaries (pub(crate))
  framing.rs  — FramingStrategy/BoundaryScanner traits + BuiltinFraming
                 (Delimiter, FixedWidth, LengthPrefixed) + FramingDescriptor
  source.rs   — ByteSource trait + MmapSource / WindowedMmapSource / PreadSource,
                 SourceMode, PlannerOptions, plan_partition_boundaries[_with],
                 plan_partition_ranges[_with], find_pattern_from (pub(crate))
  json.rs     — strict zero-dependency JSON reader (bounded depth, exact u64/u128)
  manifest.rs — FileIdentity (size/mtime/device/inode/FNV-1a sample fingerprint),
                 RangePlan + IndexReference, identify_file, plan_file[_with_framing],
                 plan_from_ranges, deterministic JSON writer + descriptor parsing
  index.rs    — RecordIndex (sparse sidecar, INDEX_SCHEMA v1), build_record_index,
                 plan_partition_boundaries_from_index (record-count balancing),
                 parse/load round-trip through src/json.rs
  ffi.rs      — C ABI: 14 public functions, ChunkLayout enum, CPartitionRange,
                 CFrameSpec, panic containment
  bin/mmap-chunker.rs — CLI: partition (TSV ranges), plan (JSON manifest,
                 optional --index), index (sidecar builder)
  plan.rs     — internal ChunkPlan state (Empty/Ranges/Fixed)

tests/
  c_abi_test.rs        — Integration test: C ABI via extern "C" (Rust calling Rust)
  benchmark.rs         — Performance: mmap vs fs::read + planner source modes
  competitive_bench.rs — Competitive: SWAR vs memchr, pattern search, chunking, partitions (Lanes A-F)
  cli_partition.rs     — CLI partition/plan/index behavior, workers, framing, source modes, errors

native_io/        — Python ctypes consumer (standalone, no harness dependency)
  contract.py       — ByteChunkProvider protocol (scan semantics; optional partition_records)
  python_provider.py — stdlib baseline (single/multi-byte delimiters, partition_records)
  mmap_provider.py  — native provider (scan_ex / scan_pattern / partition_records[_pattern])
  discovery.py      — provider discovery/selection + shadow compare
  plan_manifest.py  — plan manifest loader/verifier (mirrors the Rust fingerprint;
                      all framing strategies + indexed plans)
  record_index.py   — sparse index loader/verifier + mirrored record-count planner
tools/
  remote_verify.py  — Netcup Docker runner for heavy verification (packaging + ssh + receipt)
integrations/
  datafusion/       — standalone crate: plan ranges -> DataFusion execution partitions
mmap_chunker.h    — Public C header with full API docs
```

## Key Invariants

- **0 runtime dependencies** — no crates in `[dependencies]`
- **Read-only mmap** — `MmapFile` is immutable, `Send + Sync`; `WindowedMmapFile` remaps under a mutex
- **Panic containment** — all FFI boundaries use `catch_unwind`
- **C ABI stability** — additive changes only, ABI version via `mmap_engine_abi_version()`
- **All source backends are range-equivalent** — mmap/windowed/pread planning must produce byte-identical ranges (differential tests in `source.rs` + `cli_partition.rs` + `python_parity.py`)
- **All built-in framings are target-equivalent to their reference** — delimiter framing through `plan_partition_boundaries_with` must match the slice scanner; fixed-width and length-prefixed follow the same "smallest boundary strictly after the target" rule
- **Plan identity** — `plan_file*` identifies before and after planning; a changed source is rejected; `plan_from_ranges` re-checks the identity it was given
- **Index identity** — `build_record_index` identifies before and after scanning; `native_io.record_index` mirrors the planner and rejects stale sources
- **Immutable input contract** — file must not mutate while handle/planner lives
- **Threading**: open/scan/free = single-thread, get_chunk = multi-thread after scan
- **No harness imports** — this library has no dependency on any agent harness
- **64-bit only**: `compile_error!` at the pointer-width gate; 32-bit glibc `off_t` is 4 bytes — Rust FFI declares `i64` (8 bytes), which is an ABI mismatch (calling-convention corruption, not just truncation). musl 32-bit has 64-bit `off_t` but is untested and unsupported.
- **Integer safety**: all byte offsets and lengths are checked (`try_from`) or saturating — no silent wraparound
- **Release artifacts**: tag `vX.Y.Z` triggers `release.yml` — validates tag == Cargo.toml version, matrix-builds 5 platforms, uploads per-platform archives (header + dynamic + static lib + sha256). Draft created for manual review. crates.io publish remains separate manual step.

## Public C ABI (14 functions, ABI v1.6)

| Function                          | Purpose                              |
|-----------------------------------|--------------------------------------|
| `mmap_engine_abi_version()`       | Returns `(major << 16) \| minor`     |
| `mmap_engine_capabilities()`      | Feature detection bitmask (bits 0-8) |
| `mmap_engine_last_error()`        | Thread-local error diagnostics       |
| `mmap_engine_open(path)`          | Open + mmap file                     |
| `mmap_engine_scan_chunks(h, sz)`  | Scan with newline delimiter (v1.0)   |
| `mmap_engine_scan_chunks_ex(h,sz,delim)` | Scan with configurable single-byte delimiter |
| `mmap_engine_scan_chunks_pattern(h,sz,d,len)` | Scan with borrowed multi-byte delimiter (v1.3) |
| `mmap_engine_scan_fixed(h, sz)`   | Fixed-size arithmetic chunking (v1.1)|
| `mmap_engine_partition_records(h, n, d)` | Record-aligned N-way partition (v1.2)|
| `mmap_engine_partition_records_pattern(h,n,d,len)` | Multi-byte record-aligned partition (v1.4) |
| `mmap_engine_plan_partition_ranges(path,n,d,len,mode,window,out,cap,count)` | Source-selectable planner, no handle (v1.5) |
| `mmap_engine_plan_partition_ranges_framed(path,n,frame,mode,window,out,cap,count)` | Framing-strategy planner: delimiter/fixed-width/length-prefixed (v1.6) |
| `mmap_engine_get_chunk(h, i, out)`| Zero-copy chunk by index (returns 0/-1) |
| `mmap_engine_free(h)`             | Release all resources (abort on panic)|

Capability bits: 0 ZERO_COPY, 1 CONFIGURABLE_DELIMITER, 2 ERROR_STRINGS,
3 FIXED_SIZE_CHUNKING, 4 RECORD_PARTITIONING, 5 MULTI_BYTE_DELIMITER,
6 MULTI_BYTE_PARTITIONING, 7 WINDOWED_PLANNING, 8 FRAMING_STRATEGIES.

Source modes for the planner functions: 0 mmap, 1 windowed
(`window_bytes >= 65536`), 2 pread. Two-phase protocol: call with
`out_ranges = NULL, capacity = 0` → returns `-2` and stores the count;
allocate; call again → `0`. `-1` = error (`mmap_engine_last_error()`).

`CFrameSpec.kind`: 0 delimiter (`delimiter`/`delimiter_len`), 1 fixed width
(`record_bytes`), 2 length-prefixed (`prefix_bytes` 1..=8,
`prefix_little_endian`, `length_includes_prefix`). Full layout is documented
in `mmap_chunker.h`; the struct is 48 bytes on 64-bit targets.

## Gotchas

- **`memchr` is dev-only** (in `[dev-dependencies]`) — used only in `tests/competitive_bench.rs` for baseline comparison. Zero runtime dependencies maintained.
- **Toolchain**: rustup default `1.95-x86_64-pc-windows-gnu` has a broken cargo; use `+1.94-x86_64-pc-windows-gnu` or MSVC locally.
- Redundant `unsafe` blocks inside outer `unsafe` blocks trigger clippy `unused_unsafe`
- `extern "C" fn` that are inherently safe (no pointer access) should NOT be `unsafe`
- Cargo.lock is gitignored (library, not application) but exists in-tree from early commit
- Python native_io module requires `cargo build --release` before tests
- **Integer arithmetic**: scanner targets use `saturating_add`, partition uses `u128`, file-size uses `usize::try_from` — do not revert to unchecked `as` casts or `+`
- **Windowed mmap**: views must start at a multiple of `VIEW_ALIGNMENT` (64 KiB). Do not change the alignment without checking Windows allocation granularity and macOS 16 KiB pages. `window_bytes < 65536` is rejected.
- **Plan identity on Windows**: the planner records the 32-bit volume serial from `BY_HANDLE_FILE_INFORMATION`; CPython >= 3.12 reports the 64-bit `FileIdInfo` serial in `st_dev`. `native_io.plan_manifest` compares the low 32 bits on Windows — keep both sides in sync.
- **Fingerprint parity**: `src/manifest.rs::sample_fingerprint` and `native_io/plan_manifest.py::_sample_fingerprint` are mirrored implementations (FNV-1a over LE size + head/tail samples, whole file when samples overlap). Change both together.
- **Release workflow**: `release.yml` builds native artifacts per platform on tag push. Uses `cross-rs/cross` for Linux aarch64 cross-compilation. Artifact naming uses Rust target triples. Draft release must be published manually. Third-party actions pinned to commit SHA. Workflow fails closed — missing expected artifacts abort the job.
