### 2026-08-08 23:00 — Release preparation wave: OSS, C ABI, bugfixes
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-pro) |
| Task | Full v0.1.0 release preparation audit and implementation |
| Commit | `9312416` |
| Ergebnis | OK: All gates passed, 2 critical bugs fixed |

Bugs fixed:
- `unwrap_or({set_error(...); default})` evaluates block unconditionally before unwrap_or
  → `match catch_unwind(...)` replaces unwrap_or in scan_chunks_ex and get_chunk
- `CEngineHandle` ZST ([u8;0]) → u8: unsafe pointer cast semantics preserved

Added:
- OOB error diagnostics test (ffi/test_oob_after_iteration_newline)
- C consumer example (examples/c_consumer.c, 15 tests)
- Real C ABI E2E verification (gcc 15.2.0, static library linking)

State:
- 47 Rust tests pass (45 unit + 2 integration)
- 53 Python ctypes tests pass (native_io)
- 15 C ABI consumer tests pass
- Clean working tree (3 intentional untracked dev-only files)
- `cargo package` succeeds
- Repository URL placeholder remains (owner decision)

### 2026-08-08 23:45 — Measurement integrity + fixed-size chunking
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-pro) |
| Task | Repair benchmark measurement defects; add arithmetic fixed-size chunking |
| Commit | `d2a44ec` |
| Ergebnis | OK: Classification PROMOTE_FIXED_SIZE_ARITHMETIC, all gates pass |

Measurement defects fixed:
- search_bytes counted available remainder span (~500,000x distortion) — now counts actual examined bytes (early-exit)
- Duplicated find_byte_swar in benchmark replaced with production call
- Scanner "scalar vs SWAR" comparison was SWAR vs SWAR — now genuine scalar baseline
- Labels corrected, sample count/build mode added to output

Fixed-size feature:
- scanner::fixed_chunk_count + fixed_chunk_bounds (O(1) arithmetic, 0 deps, 0 unsafe)
- ChunkLayout enum (Empty/Delimited/Fixed) replacing Engine.chunks Vec
- mmap_engine_scan_fixed C ABI function (additive, v1.1)
- CAP_FIXED_SIZE_CHUNKING (bit 3), ABI 0x0001_0001
- C consumer: 22 tests (7 new fixed-size scenarios)
- Scanner: 27 unit tests (17 delimiter + 10 fixed-size)
- FFI: 20 unit tests (12 original + 8 fixed-size)

State:
- 84 Rust tests pass (72 unit + 2 integration + 10 benchmark correctness)
- cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo build --release: all green
- ABI v1.1 additive, no breaking changes
- Metadata: 24 bytes arithmetic vs up to 4 GiB eager at 1 TiB/4 KiB

### 2026-08-08 23:44 — First crates.io publish: v0.2.0
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-pro) |
| Task | First crates.io publish closure for mmap-chunker-core v0.2.0 |
| Commit | - (0 code commits, published from pristine tag 8dae7d5) |
| Ergebnis | OK: V0_2_0_PUBLISHED_AND_VERIFIED |

Publish details:
- Published with token-based auth by trungminhdo4-glitch (rufus)
- Pristine worktree from tag v0.2.0 (8dae7d5), not from main
- 22 files, 201.6 KiB (43.4 KiB compressed)
- All gates: fmt, check, clippy, test (108+2), doc-test, release build, --dry-run
- crates.io verified, docs.rs BUILD SUCCESS, smoke test passed
- Owner: trungminhdo4-glitch, sole owner
- trustpub_only: false — Trusted Publishing (GH Actions OIDC) recommended for future

### 2026-08-19 14:30 - DataTrove single-file adoption proof
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-flash) |
| Task | Close PR #22; prove DataTrove single-large-JSONL adoption path via mmap-chunker range manifest |
| Commit | PR #22 merge `e9fa183` (main); feature commit `19ad336` (Draft PR #24) |
| Ergebnis | OK: PR #22 merged clean; correctness 44/44; 256 MiB speedup 2.15x (4w) / 2.32x (8w); recommendation PYTHON_PACKAGE_NEXT |

PR #22 closure: body repaired (removed stale `do not merge before v0.2.4 release` / `CI run #100 green`, evidence = `CI run #103 green`), marked Ready, normal merge commit `e9fa183` (parents 6ef6b39 + d1a34a1), branch preserved, post-merge CI run #104 green.

DataTrove proof (branch `feat/datatrove-single-file-adoption-proof`): confirmed upstream per-file sharding; chose controller pre-plans once (existing `partition` CLI) + custom `RangeJsonlReader`; 44/44 correctness matrix (ids/canonical keys/checksums/range invariants/determinism); benchmark 16 MiB loses (0.68-0.92x, spawn overhead), 256 MiB wins (1.31x/2.15x/2.32x at 2/4/8 workers); fsspec `read_block` forward-skip + duplicate hazards demonstrated; no production Rust/C ABI change; isolated venv (datatrove 0.10.0 @ a649de7); Draft PR #24 open. Bundle: `target/datatrove-adoption-proof-19ad336.bundle`.

### 2026-08-19 17:15 - Python wheel distribution (mmap-chunker-core)
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-flash) |
| Task | Close PR #24; build production-shaped unpublished Python wheel around the stable C ABI; prove fresh-env pip install -> plan_file |
| Commit | PR #24 merge `af1d9d5` (normal merge, parents e9fa183+19ad336, branch kept, post-merge CI green); feature branch `feat/python-wheel-distribution` 5 commits `bcfd579..5a246b7` (Draft PR #25) |
| Ergebnis | OK: A - PYPI_RELEASE_CANDIDATE (unpublished). py3-none wheels build/install clean on all 5 targets; Python 3.10/3.12/3.14 same-wheel proven; DataTrove packaged parity + no regression; GLIBC<=2.17 restored via cross CentOS images (incl. new aarch64 pin) |

PR #24 closure: verified head 19ad336/base e9fa183 unchanged, 7 files, CLEAN merge state, 8/8 CI checks green, no reviews/threads -> marked Ready, normal merge `af1d9d5`, branch preserved, post-merge CI green.

Python package: dist name `mmap-chunker-core` (PyPI free), import `mmap_chunker`, stdlib ctypes + bundled cdylib, zero runtime deps, `plan_file` immutable Plan/Range, deterministic in-package loader with ABI v1.3 + cap-bit validation, lazy DataTrove integration (`[datatrove]` extra). Wheel CI (python-wheel.yml) builds 5 platforms, verifies ABI/GLIBC/contents, clean-venv proofs, same-wheel across 3.10/3.12/3.14, datatrove smoke; artifacts only, publication NONE. sdist rebuilds with Cargo (proven). Planning overhead: API 0.72 ms vs CLI subprocess 6.78 ms (~9.4x). CI fixes this session: wheel-inspection .data/purelib prefix, cross CentOS glibc floor for both Linux targets (runner glibc had drifted to 2.34), aarch64 runtime proof skip, Windows venv python path, datatrove pytest install. Bundle: `D:\Data Chunking\mmap-chunker-core-python-wheel-5a246b7.bundle` SHA-256 `54C32D109DFD350310A33BEF3C7F64BA7E88F1F2E531E5CB3A67939585E5DB05`.

### 2026-08-19 17:00 - v0.2.5 release prep + OIDC Trusted Publishing (PUBLISH STATE: NONE)
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-flash) |
| Task | Merge PR #25, prepare v0.2.5 as first Python-distribution release, wire PyPI + crates.io OIDC trusted-publishing automation |
| Commit | `eb44420` (release/v0.2.5, off `892235b` = new main after PR #25 merge) |
| Ergebnis | OK: PR25 merged, v0.2.5 validated, dry-run green, publication NOT performed |

- PR #25 merged as normal merge commit `892235b8` (parents `af1d9d5b`, `5a246b7`); post-merge CI green.
- Version: crate + Python dist + CLI + wheel/sdist all `0.2.5`; C ABI `0x00010003`, caps `0x3f`, MSRV 1.77 unchanged.
- python-wheel.yml reusable via workflow_call (+ sdist job); release.yml calls it, adds `publish-crate` (env crates-io, crates-io-auth-action) and `publish-pypi` (env pypi, pypa/gh-action-pypi-publish) with id-token: write at job scope; GitHub Release last.
- Dry-run workflow_dispatch success (run 32277935663): all build/verify lanes green, publish jobs skipped. GLIBC <= 2.17 + exact ABI symbols verified in CI.
- Local: full cargo suite + package/dry-run, wheel+sdist build, clean-wheel + sdist-rebuild proofs, planner/CLI/DataTrove/C-ABI parity all pass.
- Draft PR #26 opened (release/v0.2.5); 20 PR checks green.
- External owner setup pending: GitHub envs `pypi`+`crates-io`, PyPI trusted publisher, crates.io trusted publisher.

### 2026-08-20 10:20 - v0.2.5 PUBLISHED AND VERIFIED (crates.io + PyPI + GitHub Release)
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4-flash) |
| Task | Execute the authorized v0.2.5 release: push tag, publish crates.io + PyPI via OIDC Trusted Publishing, create + finalize GitHub Release, full verification |
| Commit | main `b8cdfa5` (tag v0.2.5 → `b8cdfa58061183bd3c57f13a70023f1889ec894d`); fix series `ae037e3`, `5d6c31b`, `1013975`, `a7acbe3`, `b8cdfa5` |
| Ergebnis | OK: V0_2_5_PUBLISHED_AND_VERIFIED. crates.io 0.2.5, PyPI 0.2.5 (5 wheels + 1 sdist), GitHub Release v0.2.5 Latest (20 assets) |

- PR #26 merged `446fab1` (tree == PR head `eb44420`); initial tag run 32284510179 revealed envs `pypi`/`crates-io` had `v*.*.*` registered as BRANCH policies → tag deployments rejected; fixed by adding TAG-type `v*.*.*` policies to both envs (REST).
- Release blockers found + fixed on main (each re-tagged v0.2.5):
  - `ae037e3` download-artifact `pattern` is a single glob; newline list matched 0 → two download steps (wheel-* + python-sdist).
  - `5d6c31b`+`1013975` bdist_wheel on macOS ignores plat_name and emits `macosx_*_universal2` even for thin single-arch binaries → pinned matrix tags `macosx_10_13_x86_64`/`macosx_11_0_arm64` + PlatformWheel.get_tag returns plat_name directly.
  - `a7acbe3` assert-pypi-distributions.sh used backslash-escaped version `0\.2\.5` in bash case patterns → never matched (quoted patterns treat backslashes literally); use plain version.
  - `b8cdfa5` publish-crate not idempotent → skip `cargo publish` when crates.io already reports the exact version (crates.io 0.2.5 published at `1013975`, crate source identical to `b8cdfa5`; `/scripts/` excluded from crate package).
- External setup by owner: crates.io trusted publisher (repo trungminhdo4-glitch/mmap-chunker-core, workflow release.yml, env crates-io); PyPI pending publisher for project `mmap-chunker-core` (repository field = repo NAME not URL, workflow `release.yml` not placeholder).
- Final run 32352173032 green: publish-crate success (skip), publish-pypi success, publish-release success → draft → finalized Latest.
- Verified: tag→`b8cdfa5`; crates.io max 0.2.5 (not yanked); PyPI 0.2.5 exactly 5 wheels + 1 sdist; fresh real-PyPI `pip install mmap-chunker-core==0.2.5`; installed-wheel proof `plan_file()` partitions 10000 B → 4 ranges full coverage, `abi_version()=0x00010003`, `capabilities()=0x3f`; release assets = 20 (5 native + 5 CLI archives + 10 .sha256).

### 2026-09-01 17:05 - native_io WIP verified and committed (Wave 208 §32 real work)
| Feld | Wert |
|---|---|
| Agent | OpenCode |
| Task | Native byte-chunking provider layer (untracked WIP aus feat/prebuilt-cli-distribution) verifizieren und commiten |
| Commit | (dieser Commit) |
| Ergebnis | OK: 53/53 pytest, cargo check clean, shadow_compare mmap==python byte-exakt, Coverage vollstaendig |
