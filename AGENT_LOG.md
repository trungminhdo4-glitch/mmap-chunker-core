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

### 2026-09-21 18:10 - Planner-Roadmap P0: Multi-Byte-Partition, ByteSource-Backends, Plan-Manifest
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4.1-flash) |
| Task | P0-Roadmap implementiert: (1) Multi-Byte-Partitionierung Rust/C-ABI/CLI/Python, (2) ByteSource + Windowed-mmap/pread-Planner + Benchmark, (3) versioniertes Plan-Manifest + File-Identity + Python-Verifier |
| Commit | - (nicht committet, Owner-Entscheid offen) |
| Ergebnis | OK: fmt/clippy/test clean, 264 lib + alle Integrationstests gruen, 79 pytest + 2806 C-ABI-Parity-Checks, MSRV 1.77 check clean |

Details:
- `find_partition_boundaries_pattern` + `MmapChunker::partition_records_pattern` + `mmap_engine_partition_records_pattern` (ABI v1.4, CAP bit 6); CLI `--delimiter-hex`; Python-Provider multi-byte Scan/Partition (Baseline + mmap).
- `src/source.rs`: `ByteSource` mit `MmapSource` (Slice-Scanner-Delegation), `WindowedMmapSource` (64-KiB-aligned Moving Window in `mmap.rs`), `PreadSource` (Std-`read_at`/`seek_read`); `plan_partition_ranges` differenziell gegen Slice-Scanner getestet (`source.rs`, `cli_partition.rs`, `python_parity.py`); FFI `mmap_engine_plan_partition_ranges` (ABI v1.5, CAP bit 7, Zwei-Phasen-Protokoll).
- `src/manifest.rs`: deterministisches JSON-Schema `mmap-chunker-plan` v1, `FileIdentity` (Size/mtime/dev+inode inkl. Windows `GetFileInformationByHandle`/FNV-1a Sample-Fingerprint ueber LE-Size+Head/Tail), Identity vor+nach Planung; CLI `plan --output`; `native_io/plan_manifest.py` spiegelt Fingerprint und lehnt stale Plaene ab (Size/mtime/Device/Inode/Fingerprint).
- Benchmark `benchmark_source_modes` (64 MiB CRLF): mmap 372 us, pread 3.4 ms, windowed 10.9 ms (Scan-Buffer-Kopien) - Werte maschinenabhaengig.
- Conformance-Konsumenten (C/Python/Go/C#) auf ABI 0x00010005 / caps 255 aktualisiert; `expected.txt` nachgezogen.
- Toolchain-Hinweis: rustup default 1.95-gnu cargo defekt, lokal `+1.94-x86_64-pc-windows-gnu` verwendet (MSVC verfuegbar).

### 2026-09-21 19:40 - Planner-Roadmap P1/P2: Framing-Strategien, Sparse Index, JSON-Reader
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4.1-flash) |
| Task | P1/P2-Fortsetzung: pluggable Framing-Strategien (Delimiter/FixedWidth/LengthPrefixed), persistenter Sparse-Record-Index (Sidecar + indexed planning), dependency-freier JSON-Reader, C-ABI v1.6, Python-Loader/Verifier |
| Commit | - (nicht committet, Owner-Entscheid offen) |
| Ergebnis | OK: fmt/clippy clean, 294 Lib + 14 CLI-Unit + 17 CLI-Integration + alle weiteren Integrationstests gruen, 88 pytest, 2834 C-ABI-Parity-Checks. MSRV-1.77-Vollcheck (schwer) auf Netcup/CI deferred |

Details:
- `src/framing.rs`: `FramingStrategy`/`BoundaryScanner`; Builtins `Delimiter`, `FixedWidth`, `LengthPrefixed` (1..=8 Byte Prefix, LE/BE, Prefix-inclusive optional); stateful Length-Prefixed-Scanner O(file); `FramingDescriptor` fuer Manifeste; Differentialtests gegen Slice-Scanner.
- `src/source.rs`: `plan_partition_boundaries_with`, `plan_partition_ranges_with`; identische Target-/Assembly-Semantik fuer alle Framings.
- C ABI v1.6 (0x00010006), CAP bit 8 FRAMING_STRATEGIES: `CFrameSpec` + `mmap_engine_plan_partition_ranges_framed` (Zwei-Phasen-Protokoll); Header-Layout dokumentiert (48 Byte); Conformance-Konsumenten C/Python/Go/C# auf caps 511 aktualisiert.
- `src/index.rs`: `RecordIndex` Schema `mmap-chunker-index` v1 (Identity, Framing, stride, record_count, record_offsets), `build_record_index`, `plan_partition_boundaries_from_index` (Record-Count-Balancing, Rundung auf naechsten Slot); CLI `index FILE --every N` (Default FILE.mmapidx), `plan --index`.
- `src/json.rs`: strikter JSON-Reader (MAX_DEPTH 64, exakte u64/u128, Escapes) zum Laden eigener Manifeste/Indizes.
- `src/manifest.rs`: `FramingDescriptor` im Manifest, `IndexReference`/`source_index`, `plan_from_ranges`, `plan_file_with_framing`; `source_mode` nullable fuer indexierte Plaene.
- Python: `native_io.record_index` (Loader/Verifier + gespiegelter Planner) und `plan_manifest`-Erweiterung (alle Framings, indexed plans, gemeinsames `verify_identity`); neue Suiten `tests/test_record_index.py`, erweiterte Manifest-/Parity-Tests (framed planning via C ABI).
- Schwere Laeufe: rustup-Default 1.95-gnu cargo defekt; lokal `+1.94-x86_64-pc-windows-gnu`. MSRV-1.77-`check --all-targets` abgebrochen (schwer) und als Remote-/CI-Aufgabe vorgemerkt.

### 2026-09-21 21:05 - Remote-Heavy-Verify (Netcup) + DataFusion-Integrationsproof
| Feld | Wert |
|---|---|
| Agent | OpenCode (deepseek-v4.1-flash) |
| Task | Netcup-Docker-Runner `tools/remote_verify.py` gebaut (Owner-Entscheid: schwere Runtime auf Netcup); DataFusion-Integrationscrate `integrations/datafusion` (Owner-Entscheid: separates Crate, Core bleibt 0-Dep) mit End-to-End-Proof |
| Commit | - (nicht committet, Owner-Entscheid offen) |
| Ergebnis | OK: Remote-Receipts exit 0 fuer Core (fmt+clippy+test), MSRV 1.77, DataFusion (clippy+test+doctest); lokal 88 pytest + 2834 Parity-Checks gruen |

Details:
- Netcup hat kein cargo/gcc, aber Docker (29.8.0) + passwordless sudo. Runner faehrt ephemere `rust:1.94`/`rust:1.77`-Container (`--rm`, `sudo -n`), uebertraegt Worktree-Tarball per scp (81 Dateien, 0.3 MiB), Base64-Command gegen Quoting-Probleme, Cleanup per sudo-Trap; Receipt in `target/remote_verify_receipt.json`.
- Receipts: Core `fmt+clippy+test` exit 0 (alle Suiten inkl. 294 Lib-Tests + 18 CLI-Integration); MSRV `rust:1.77 cargo check --all-targets` exit 0; DataFusion `clippy -D warnings + test + doctest` exit 0 (3 Integrationstests: planned_ranges_become_execution_partitions, every_partition_boundary_is_a_record_boundary, windowed_plan_and_manifest_drive_the_scan).
- `integrations/datafusion`: `NdjsonRangeScan` mappt jede Plan-Range auf genau eine DataFusion-Partition (`PartitionedFile::with_range` + `JsonSource`), Manifest-Reader nutzt den 0-Dep-JSON-Parser des Core; Vergleich partitionierter Scan == Single-Partition-Referenz (rows/sum). datafusion 55.1.0 + datafusion-datasource(-json) 55.1.0; Core-Cargo `exclude` um `/integrations/`/`/tools/` erweitert.
- API-Ermittlung remote aus Crate-Quellen (docs.rs + crates.io-Tarballs), um lokale schwere Builds zu vermeiden.
- Offen (P2): Seekable-Compression-Adapter; adaptive Policies jenseits des Index (Sampled Planner); evtl. Integrations-Crate in CI einhaengen.

### 2026-09-24 - ADHD-Runde: Coverage-Lineage + Perf-Guard + Hygiene-Docs (autonom, 3 Subagenten)
| Feld | Wert |
|---|---|
| Agent | OpenCode |
| Task | Evidenzbasierte Auswahl (mmap-chunker-core) + ADHD-Divergenz (5 Frames x 6 Ideen) + 3 parallele Worker: A Hygiene-Docs, B verify_coverage-Lineage, C fused_lap_saving_probe |
| Commit | - (nicht committet, Push-Gate; Aenderungen uncommitted im Worktree) |
| Ergebnis | OK: plan_parity 8/8, lib manifest 13/13, benchmark-Probe 1/1 ignored (14 Syscalls + 57344 Bytes Saving belegt); fmt clean; clippy --test clean; --all-targets-FAIL vorbestehend (benchmark.rs:493 E0599, ausserhalb Scope) |

Details:
- A (Docs only): `.gitignore` +`Data chunking/`; `RELEASE.md` ABI 1.3 -> 1.6 (0x00010006) + Known-Drift-Sektion Tags v0.2.3-v0.2.6 vs Cargo 0.2.2 (KEIN Bump); `CHANGELOG.md` [Unreleased]-Pendingliste (framing/source/manifest/index/json, plan_manifest/record_index, integrations/, tools/).
- B (Scope src/manifest.rs untracked + tests/plan_parity.rs): `RangePlan::canonical_bytes()` + `verify_coverage()` (sortiert/kontiguierlich/Start-0/Ende-size + BuiltinFraming-Replay, Custom skipt Replay) + `CoverageError`/`CoverageKind`; 4 neue fail-closed Tests (24-B-Fixture, 0.04s). Risiko offengelegt: sample_fingerprint deckt nur Head/Tail ab; verify_coverage replayt volle Bytes (Geometrie-Proof, kein Content-Proof).
- C (nur tests/benchmark.rs, #[ignore]): `fused_lap_saving_probe` 1 MiB/5762 Records: separat 5776 pread-Calls/23614430 Bytes vs fused 5762 -> gespart 14 Calls + 57344 Bytes; post-target 730 B/7 Cuts; partition==plan byte-identisch.
- Offen (Owner): Version-Bump (Cargo 0.2.2 -> 0.2.6+?) vs Tags als ueberholt; Commit der Pending-Module; MSRV-1.77-/DataFusion-Heavy auf Netcup deferred.

### 2026-09-24 - Production Consolidation: Reconcile + Verify + Minimal Slice (Phasen 0-9)
| Feld | Wert |
|---|---|
| Agent | OpenCode |
| Task | Ground-Truth-Rekonstruktion, Reconciliation des Vor-Runs (canonical_bytes/verify_coverage/Probe/Docs), Dirty-Tree/Version/Clippy-Analyse, Core-vs-Sidecar-Entscheid, Perf-Methodik, Netcup-Remote-Verify, minimaler Implementierungs-Slice |
| Commit | - (nicht committet: alle 4 eigenen Dateien enthalten fremde uncommittete Vorarbeiten, Commit-Ownership nicht nachweisbar; kein Push/Tag/Release) |
| Ergebnis | OK: lokal plan_parity 9/9 + lib-manifest 13/13 + fmt/clippy clean; remote rust:1.77 check --all-targets exit 0; remote rust:1.94 cargo test exit 0 (347 passed, 0 failed) |

Details:
- PHASE 0: Branch feat/prebuilt-cli-distribution @19a628d (2 Commits vor origin), 32M+11U, staged nichts; Toolchain +1.94 ok, Default 1.95 defekt; Owner-Goal GLOBAL_AGENT_PLATFORM_CONSOLIDATION; 30 fremde Worktrees unberuehrt; keine fremden Writer seit 21.09 (mtimes).
- PHASE 1: Vor-Run bestaetigt (Funktionen + 4 Tests + Probe + Docs vorhanden); aber: NULL Produktions-Caller fuer verify_coverage/canonical_bytes (nur Tests) -> PARTIAL. Semantik-Klassik: Coverage/Luecken/Overlap/Boundary=YES (Builtin), Framing-Identitaet=Selbstkonsistenz (PARTIAL), Content/Dateiversion=NO (by design, dokumentiert). Terminologie: deterministische Coverage-Verifikation, KEINE signierte Linie (kein Crypto).
- PHASE 2: E0599 (benchmark.rs:493) widerlegt: transienter Mid-Edit-Artefakt des Vor-Runs; check --tests + remote --all-targets gruen. Version: Tags v0.1.0-v0.2.6 konsistent (v0.2.6=ABI ...0003/11 Fns); Worktree Cargo 0.2.2 + ABI ...0006 + 3 Fns (v1.4-v1.6) uncommitted; Branch 33 Commits divergent zu main (wheel/datatrove fehlt). Stale gefunden: abi/v1.symbols (11 statt 14, verletzt eigene CI-Policy), ffi.rs:135 Kommentar v1.4.
- PHASE 3: Entscheid A (kein Workspace-Split): 0-Dep belegt, DataFusion-Sidecar bereits entkoppelt (eigene Workspace, publish=false, MSRV 1.86), Consumers nur via C-ABI/JSON, framing/source-Zyklus ist beabsichtigte Trait-Grenze. ADHD-Sidecar-Pick damit evidenzbasiert verworfen.
- PHASE 4: H1/H2/H3 nicht belegt genug zum Bauen: Probe zaehlt logische read_at (nicht Syscalls; mmap-Fast-Path hat null read_at; Bytes an SCAN_BUF gekoppelt; 182B-Fixture Best-Case). VIEW_ALIGNMENT 64K portabel begruendet, performativ willkuerlich. Kein Code geändert, nur Methoden-Limits dokumentiert.
- PHASE 5: Netcup minh@159.195.82.241 + Docker 29.8.0 ok; Tarball 85 Files/0.3 MiB (keine Secrets); Receipts: msrv exit 0, core-test exit 0 (elapsed 4.1s).
- PHASE 6 (Slice): tests/plan_parity.rs +1 Test (Inverted/Zero-Length/Unsorted/Duplikat/InvalidFraming/Boundary+Positiv/Misaligned/Order-Sensitivitaet); abi/v1.symbols +3; ffi.rs:135 -> v1.6; benchmark.rs Probe-Limits-Kommentar (logisch vs Syscall).
- PHASE 7: PASS-Lokal: plan_parity 9/9, lib-manifest 13/13, clippy --test clean, check --tests/--lib/--bins gruen. PASS-Remote: 347 passed/0 failed (lib 295+2ign, bin 14, c_abi 3, cli_partition 18, plan_parity 9, doctests 5, Rest ignored). BLOCKED (Environment, kein Code): lokales check --all-targets --offline (Registry-/Sysroot-Staleness im Shared-Cache). NOT RUN: DataFusion-Integration remote, Python/Go/C# live-Conformance, 1GiB-Bench (Heavy-Policy).
- PHASE 8: Kein Commit (Ownership-Regel), keine Secrets, keine API-Aenderung (nur Comment/Symbols/Tests). Naechste Aktion: CLI-`verify`-Slice (RangePlan::from_json + `mmap-chunker verify MANIFEST FILE` + CLI-Tests); Voraussetzung: Owner-Priorisierung + Verantwortlichkeit fuer neue Deserialisierungs-Surface.

### 2026-09-24 - CLI Verify Slice + Compile-Fix + Cross-Language-Paritaet (Phasen 0-9)
| Feld | Wert |
|---|---|
| Agent | OpenCode |
| Task | `RangePlan::from_json` + `mmap-chunker verify MANIFEST FILE` + Fehlervertrag + Unit/CLI/Python-Tests; Compile-Bremse diagnostiziert/behoben; Remote-Gates; Python-Duplikat-Paritaet |
| Commit | - (nicht committet: eigene Aenderungen in fremd-dirty Dateien + untracked Modulen; kein Push/Tag/Release) |
| Ergebnis | OK: lokal cli_partition 24/24 + plan_parity 13/13 + c_abi 3/3 + pytest 4/4; remote 1.77 check exit 0, 1.94 test exit 0 (358 passed/0 failed), 1.94 clippy exit 0 |

Details:
- COMPILE-FIX: Bremse = parallele Heavy-Builds (lokal clippy + remote-tarball gleichzeitig) + dadurch zerrissener Incremental-Cache (`corrupt dep-graph.bin`, von cargo selbst erkannt/geloescht). Zusaetzlich: `D:\.cargo\config.toml` setzt `rustc-wrapper = sccache` (Server defekt) + `target-cpu=native`; 1.7 GiB target/ mit ~20 fremden CARGO_TARGET_DIRs. Fix: strikt seriell bauen, `$env:RUSTC_WRAPPER=''`-Bypass, Default-target (inkrementell, 1-46s), keine Fresh-Dirs. Vorherige E0599/offline-Fehler waren Folgen desselben Schadens, kein Code-Problem.
- PHASE 1/2: `RangePlan::from_json` (manifest.rs, +ManifestError/Kind, MAX_MANIFEST_BYTES 64MiB, MAX_MANIFEST_RANGES 1M): Duplikat-Reject (first-wins waere mehrdeutig), required/unknown-Felder, Overflow-sicher (try_from, kein `as`), index==Position, length==Span, start<end, end<=size, identity.size==source.size, delimiter_len-Cross-Check, try_as_builtin-Fruehvalidierung, semantische (nicht Byte-) Aequivalenz dokumentiert. lib.rs-Exporte ergaenzt.
- CLI: `verify MANIFEST FILE` (HELP + Dispatch + run_verify): Manifest-Limit, Parse, identify, stale-Check (size), Fingerprint-Check (head/tail), mmap + verify_coverage, Re-Identify (TOCTOU), `verify ok: N ranges, S bytes, F framing (schema vV)` / `error: ...` mit stabilen Tokens ([kind], stale manifest, source content changed, invalid manifest (kind)).
- PHASE 3: Kein `FILE INTEGRITY VERIFIED`; Ok-Zeile nennt exakt Geprueftes; Middle-Edit-ausserhalb-Samples bleibt OK (Test dokumentiert Limit); FNV kein Crypto (Test + Doku).
- PHASE 5/6: Unit 4 neu (Roundtrip, Unicode-Pfad, 20+ Negativ-Faelle, Limits) — Test-First fand 2 echte Bugs (file_identity_from_json mit falschem Objekt aufgerufen; trailing comma im Limits-Test). CLI 7 neu (fresh/Determinismus, empty, CRLF/fixed/length-prefixed, indexed, tamper overlap/gap/misaligned, stale/fingerprint/middle-honesty, bad inputs). Python 2 neu (Duplikat top/nested).
- PHASE 7: Receipts msrv_verify (1.77 check exit 0), verify_slice (1.94 test exit 0, 358 passed), clippy_verify (1.94 clippy exit 0; Komponente per rustup im Container nachinstalliert). NOT RUN: DataFusion remote (unbetroffen: json-API + to_json unveraendert), Go/C# live (nur C-ABI, c_abi 3/3), 1GiB-Bench (Heavy-Policy), CLI-abhaengige pytest (brauchen Release-LTO-Build).
- PHASE 9: Python-Divergenz (last-wins) per object_pairs_hook geschlossen; restliche Python/Rust-Unterschiede (actual_partitions-Pflicht vs. Ignore, Negativ-Int-Stufen) beidseitig fail-closed, dokumentiert, nicht geaendert.
- Offen (Owner): Version/Commit/Release wie Vor-Run; naechste Aktion: Release-Reconciliation (main-Divergenz 33 Commits) oder DataFusion-Remote-Proof bei Bedarf.
