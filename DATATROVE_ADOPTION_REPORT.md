# DataTrove single-file adoption proof — decision report

**Date:** 2026-08-19
**Branch:** `feat/datatrove-single-file-adoption-proof`
**Starting `origin/main`:** `e9fa1839847bec907aff5c03ac5b3c3205720f01` (after PR #22)
**DataTrove tested:** `0.10.0` at commit `a649de79c14a550dc90f48a15c025f2dd3fd3b57` (editable install)
**mmap-chunker:** `v0.2.4` CLI (`target/release/mmap-chunker`, Rust `cargo build --release`)
**Host:** Windows 11, Python 3.12.6, Intel i7-8700 (6 cores / 12 threads), isolated venv

## Question

Can mmap-chunker-core turn one large immutable local JSONL file into useful
parallel DataTrove work more correctly or efficiently than DataTrove's current
file-level sharding, with low enough integration friction to justify the next
adoption investment?

## Confirmed upstream sharding limitation

DataTrove shards strictly **per file**:

* `DataFolder.get_shard` returns `all_files[rank::world_size]`
  (`src/datatrove/io.py:180`) and `get_shard_from_paths_file` filters
  `(pathi - rank) % world_size == 0` (`src/datatrove/io.py:411`).
* `BaseDiskReader.run` owns one file per task; `read_file` is invoked once per
  file (`src/datatrove/pipeline/readers/base.py:184-222, 224-252`). No reader
  accepts byte offsets; nothing in `readers/` splits a file across ranks.
* `LocalPipelineExecutor.world_size == tasks` (`src/datatrove/executor/local.py:169`);
  with one input file and `tasks > 1`, extra ranks get an empty shard
  (warning only) — the file is **not** split.

One input file therefore never becomes multiple normal reader tasks. This
limitation is what the range-backed reader addresses.

## Architecture candidates

| Approach | Description | Verdict |
|---|---|---|
| **A** | Every rank invokes `mmap-chunker partition --worker K` | Rejected: repeated full planning/scanning by every rank |
| **B** | Controller pre-plans once; workers receive immutable ranges | **Chosen** |
| **C** | Direct Python ctypes/native ABI reader | Deferred: library discovery + packaging story belongs to a future Python package wave |
| **D** | Modify DataTrove upstream reader internals | Not needed; a custom reader is the idiomatic DataTrove extension point |

**Selected architecture (B):** the controller runs the existing
`mmap-chunker partition FILE --parts N` CLI exactly once, parses the
four-column TSV into a deterministic, pickle-safe manifest of immutable
record-aligned ranges `[start, end_exclusive)`, and a custom
`RangeJsonlReader(BaseDiskReader)` gives each rank its range via DataTrove's
own `(rank, world_size)` interface. No new Rust API was added; the existing
`partition` contract is used unchanged.

## Contract

Supported (the only contract mmap-chunker can safely guarantee):

* local, regular, immutable files only (no remote / object-store)
* uncompressed JSONL/NDJSON, newline-delimited records, UTF-8 content
* byte ranges `[start, end_exclusive)`; no record crosses task ownership
* each source record belongs to exactly one rank
* missing final newline; empty files; very long records; malformed-JSON
  skip behaviour identical to DataTrove

Explicitly unsupported / declined (no silent fallback):

* compressed files (gzip/zstd), remote/object-store paths
* CSV semantics; non-UTF-8 payloads; lone-`\r` line terminators (old-Mac)
* Windows note: DataTrove's `JsonlReader` opens text files with the **locale
  codec**; UTF-8 content therefore requires `PYTHONUTF8=1` on Windows (or a
  UTF-8 locale) for the baseline to read the file at all.

## Correctness matrix (44/44 pass)

Deterministic synthetic fixtures (seeded, no private data). For every case the
single-task `JsonlReader` oracle and the range-backed reader were compared on:

* logical record count
* exact IDs (default `path/line-index` with **global** line index) and explicit ids
* canonical content keys (sha256 of sorted `{text, metadata}`)
* ordered text equality, numeric aggregate, combined content checksum
* no duplicates / no missing records
* range invariants: in-bounds, contiguous/no-overlap, full coverage, record-aligned
  boundaries (each start follows a newline; each non-final range ends on a newline)
* deterministic repeated plans (byte-identical TSV + equal manifest)

| Fixture | Sizes (bytes) | Workers | Result |
|---|---|---|---|
| empty file | 0 | 1/2/4/8 | PASS |
| one record (+/– final newline) | 33 | 1/2/4 | PASS |
| many records LF | 147,889 | 1/2/4/8 | PASS |
| many records, missing final newline | 147,889 | 1/2/4/8 | PASS |
| Unicode (CJK/Greek/accents/emoji) | 32,899 | 1/2/4/8 | PASS |
| varied record sizes (1 B – 5 KiB) | 1,208,563 | 1/2/4/8 | PASS |
| one 1 MiB record spanning several ideal targets | 1,048,766 | 1/2/4/8 | PASS (partitions collapse to 2) |
| highly skewed sizes | 648,480 | 1/2/4/8 | PASS (4 actual partitions at 8) |
| requested tasks > actual partitions (2 records, 8 tasks) | 46 | 8 | PASS |
| explicit ids | 21,714 | 1/2/4 | PASS |
| malformed JSON line (skip parity) | 583 | 1/2/4 | PASS |
| CRLF separators (universal-newline parity) | 42,285 | 1/2/4 | PASS |

The range reader reproduces DataTrove document semantics exactly (default
adapter, `file_path` metadata, per-line orjson with warning-and-skip,
base64 `media_bytes`, empty-text skip).

## Benchmark methodology

* Fixtures generated deterministically (`orjson` JSONL, 200–400 char text).
* Each config: one discarded warm-up, then N timed samples, **round-robin
  interleaved** across configs to cancel machine drift; **medians** reported.
* `e2e_median_s` = full `LocalPipelineExecutor` wall time (includes spawn /
  Manager / pickling overhead — the real integration cost), `start_method="spawn"`.
* Baseline = DataTrove `JsonlReader` on the same single file (tasks=1, workers=1).
* Range-backed = `RangeJsonlReader` (tasks=workers=W, each rank one range).
* fsspec transport = same manifest ranges read through `fsspec` open/seek/read.
* Planning wall time (Rust `partition` subprocess) and manifest construction
  (offsets pass) are measured separately per worker count.

## Results (medians, 5 samples)

### Smoke 16 MiB (51,633 records)

| Config | e2e median (s) | speedup vs baseline | docs exact |
|---|---|---|---|
| DataTrove baseline | 1.252 | 1.00 | ✓ |
| range 1w | 1.355 | 0.92× | ✓ |
| range 2w | 1.770 | 0.71× | ✓ |
| range 4w | 1.611 | 0.78× | ✓ |
| range 8w | 1.843 | 0.68× | ✓ |
| fsspec 1w / 2w / 4w / 8w | 1.29 / 1.67 / 1.56 / 1.85 | 0.97 / 0.75 / 0.81 / 0.68 | ✓ |

### Standard 256 MiB (821,729 records)

| Config | e2e median (s) | records/s | MiB/s | speedup | plan s | manifest s | docs exact |
|---|---|---|---|---|---|---|---|
| DataTrove baseline | 10.211 | 80.5k | 25.1 | 1.00 | – | – | ✓ |
| range 1w | 12.303 | 66.8k | 20.8 | 0.83× | 0.007 | 0.40 | ✓ |
| range 2w | 7.821 | 105.1k | 32.7 | **1.31×** | 0.007 | 0.76 | ✓ |
| range 4w | 4.745 | 173.2k | 53.9 | **2.15×** | 0.007 | 0.82 | ✓ |
| range 8w | 4.400 | 186.8k | 58.2 | **2.32×** | 0.006 | 0.40 | ✓ |
| fsspec 1w / 2w / 4w / 8w | 12.14 / 7.38 / 4.95 / 4.18 | — | — | 0.84 / 1.38 / 2.06 / 2.45 | – | – | ✓ |

### Interpretation

* **16 MiB:** parallelism loses — spawn/Manager overhead (~0.4–0.6 s) exceeds
  any parallel gain on a sub-second-to-2-second workload (0.68–0.92×). The
  range-backed path is only useful at larger sizes on this machine.
* **256 MiB:** 2w = 1.31× (useful signal), 4w = 2.15×, 8w = 2.32× (strong local
  signal). Scaling is sub-linear (8 workers ≈ 4 workers) on this 6-core host.
* **Single-worker range ≈ baseline** (0.83–0.92×): the reader-path difference
  is real but small; the win is parallelism, not the transport.
* **fsspec transport ≈ plain transport** (2.06–2.45× vs 2.15–2.32×): given the
  same manifest, fsspec adds no meaningful cost. The differentiation is not the
  byte-read transport — it is the planning/manifest.

No numbers were cherry-picked; a slower/losing configuration (small files) is
reported rather than hidden.

## fsspec / Dask comparison

fsspec already supports delimiter-aligned block reads (`read_block`), so
**delimiter-aligned byte splitting is not novel** and this report does not
claim it. However, the boundary semantics differ materially:

* `read_block` aligns both ends **forward** (`seek_delimiter` seeks to the
  first delimiter at/after the requested offset). Demonstrated: requesting a
  block at the start of record 5 returns records `[6, 7, 8, 9]` — it **skips
  the first record** of any mid-file range, so it cannot reproduce an exact
  `[start, end)` range from a manifest.
* Naive arithmetic tiling with `read_block` is not coverage-exact: fuzzing 300
  random layouts (seed 7) found a duplicate case, reproduced deterministically
  in the proof (24 records, 4 workers → record 11 read by workers 1 and 2).

What mmap-chunker adds over plain fsspec tiling is exactly the product
differentiation: a **deterministic, complete, non-overlapping range manifest
with explicit worker ownership** for immutable local files — plus local
mmap-backed planning, cross-language reuse, the C ABI, and multi-file logical
planning (`partition-files`). If a user only needs one file read by one process,
fsspec/DataTrove is simpler and equally good; the value appears when a single
large file must be split across workers with exact ownership.

## Unsupported cases (explicit)

* compressed / remote / object-store inputs (declined, no fallback)
* CSV or quoting semantics (raw LF byte framing only)
* non-UTF-8 payloads; lone-`\r` terminators
* Windows without `PYTHONUTF8=1` for UTF-8 baselines (DataTrove text-mode
  locale codec)

## Production Rust / C ABI impact

**None.** No Rust source, C ABI, capability bits, ABI version, dependency,
release workflow, or package contract changed. The proof uses only the existing
`partition` CLI contract and pure-Python glue.

## Recommendation

**A — PYTHON_PACKAGE_NEXT.**

Evidence: correctness is strong (44/44), the benchmark signal is credible
(2.15–2.32× at 256 MiB, 4–8 workers), the integration is small and idiomatic
(a custom reader is a natural DataTrove extension point), and the primary
remaining friction is **installation**: the proof needs the standalone CLI
built from source plus a DataTrove environment, and the reader is glued
together by a plain Python module.

The next wave should deliver a dedicated Python distribution/wheel that
bundles the native library/CLI (ctypes-based, mirroring the existing
`examples/jsonl_multiprocessing_proof.py` C ABI consumer), exposing the planner
as an installable API so `RangeJsonlReader` becomes a `pip install` dependency
rather than a hand-rolled subprocess. Once that packaging exists, a
**DATATROVE_UPSTREAM_CANDIDATE** (option B) contribution becomes feasible and
is the natural follow-on: the reader design fits DataTrove's extension model
without invasive internals, but it cannot be a dependency-free upstream reader
until the native library ships as a wheel.

Not chosen: **C** (mmap/address-space was not the blocker), **D** (upstream
modification unnecessary), **E** (correctness is robust with the existing
manifest contract).

## How to reproduce

```sh
cargo build --release
python -m venv .dtvenv && .dtvenv/Scripts/python -m pip install -e <datatrove clone> orjson
# Windows: set PYTHONUTF8=1 (UTF-8 baseline); start_method=spawn is used
python examples/datatrove_single_file_proof.py --mode all --out report.json
pytest tests/test_datatrove_range_reader.py -v   # skipped without datatrove/CLI
```
