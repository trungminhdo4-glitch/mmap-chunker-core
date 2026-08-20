# DataTrove streaming parity and real parallelism proof

**Date:** 2026-08-20
**Classification:** `OWN_ADAPTER_FIX_REQUIRED_BEFORE_UPSTREAM`
**Scope:** local, immutable, uncompressed UTF-8 JSONL/NDJSON only

This wave confirms that current DataTrove still shards one local file at file
boundaries, reproduces defects in the published `mmap-chunker-core==0.2.5`
adapter, repairs those defects on a feature branch, and proves real
multi-worker execution with `LocalPipelineExecutor`. No release, tag, publish,
DataTrove PR, or DataTrove issue comment was made.

## 1. Starting state

The isolated worktree used for this wave is
`feat/datatrove-streaming-parity`, based on `origin/main` at
`b8cdfa58061183bd3c57f13a70023f1889ec894d`. `origin/main` is the published
`mmap-chunker-core v0.2.5` state; tag `v0.2.5` peels to the same commit. The
root checkout was dirty on an unrelated branch, so it was not touched.

Live GitHub reconciliation found no open PRs or issues in the own
`mmap-chunker-core` repository. The published `v0.2.5` release is present with
platform archives and SHA-256 sidecars; no artifact was downloaded.

The published package exposes `plan_file()` and
`mmap_chunker.integrations.datatrove.RangeJsonlReader`. The release contains
the existing Rust/C ABI and Python distribution; this wave did not change the
ABI, version, dependencies, tag, or release artifacts.

## 2. Live DataTrove reconciliation

Tested DataTrove source SHA:
`a649de79c14a550dc90f48a15c025f2dd3fd3b57`.

The tested package was `datatrove==0.10.0`; the live `main` SHA above was
checked separately. The [0.10.0 documentation](https://pypi.org/project/datatrove/0.10.0/)
still recommends multiple medium-sized files: one task processes one file,
and a one-file input does not become N file shards automatically. The relevant
implementation is in [DataTrove `BaseDiskReader`](https://github.com/huggingface/datatrove/blob/a649de79c14a550dc90f48a15c025f2dd3fd3b57/src/datatrove/pipeline/readers/base.py),
[JSONL reader](https://github.com/huggingface/datatrove/blob/a649de79c14a550dc90f48a15c025f2dd3fd3b57/src/datatrove/pipeline/readers/jsonl.py),
[file sharding](https://github.com/huggingface/datatrove/blob/a649de79c14a550dc90f48a15c025f2dd3fd3b57/src/datatrove/io.py), and
[LocalPipelineExecutor](https://github.com/huggingface/datatrove/blob/a649de79c14a550dc90f48a15c025f2dd3fd3b57/src/datatrove/executor/local.py).

Issue [#74, “In-file parallelism”](https://github.com/huggingface/datatrove/issues/74)
remains open and relevant. Issue [#206, the large single-file/megawarc case](https://github.com/huggingface/datatrove/issues/206)
also remains open. The current open PR list was checked; no native in-file
parallelism implementation or duplicate merged/open PR was found. The
de-facto workaround remains physically splitting the input into files. The
DataTrove [contribution rules](https://github.com/huggingface/datatrove/blob/main/AGENTS.md)
also require maintainer coordination before adding a dependency, so no
DataTrove dependency change was attempted.

## 3. Published `0.2.5` ground truth

An isolated venv installed from real PyPI:

```text
mmap-chunker-core==0.2.5
datatrove[io]==0.10.0
orjson
```

On a ten-record fixture with a four-part plan, the published wheel produced:

| Case | Published result |
|---|---:|
| normal | 10 documents |
| `skip=2` | 0 documents; loop never advances its skip counter |
| `limit=1` | 10 documents; limit is ignored |
| `limit=5` | 10 documents |
| `skip=2, limit=5` | 0 documents |

The published adapter also reads `fh.read(assignment.length)` and constructs
`segment = mapping[r.start:r.end]` for offset counting. A 4+ MiB range showed
`tracemalloc` peak of approximately 16.94 MiB in the published reader. This is
range-size-dependent user-space materialization, not a production-safe
streaming contract.

## 4. Hypothesis audit

| Hypothesis | Result | Evidence / disposition |
|---|---|---|
| H1: `skip` permanently continues | **Confirmed** | Published `0.2.5` never increments the relevant counter. Fixed and regression-tested. |
| H2: `limit` parity gap | **Confirmed** | Published `0.2.5` has no effective limit check. Fixed for DataTrove's per-task semantics; `limit=0/1/5`, `skip`, and `skip+limit` are tested against a one-task oracle. |
| H3: whole-range RAM copy | **Confirmed** | `fh.read(assignment.length)` removed. Reader now seeks and uses binary `readline()`. |
| H4: offset slices copy ranges | **Confirmed** | `mapping[r.start:r.end]` removed. Offset construction now uses 1 MiB bounded reads. |
| H5: benchmark is serial | **Confirmed** | The old `_range_docs()` helper directly called ranks serially and was not speed evidence. A real executor proof now uses `LocalPipelineExecutor(tasks=N, workers=N)`. |
| H6: plan/world-size mismatch | **Confirmed** | Published behavior was implicit and could silently leave ownership gaps. The adapter now requires `world_size == plan.requested_parts`; extra collapsed ranges remain explicit empty assignments. |
| H7: stats/document parity | **Partially confirmed, repaired** | Default/custom adapter options, IDs, metadata, document counts, `doc_len`, logical `input_files`, warnings, and malformed-line behavior are covered. `skip`/`limit` remain per-task as in `BaseDiskReader`; use a one-part plan for a whole-file global skip or limit. |

`input_files` is counted once for the logical source file in aggregate range
stats; `documents` and `doc_len` are additive across ranges. Runtime tracking
was restored with `track_time()`.

## 5. Differential parity matrix

Oracle: current DataTrove `JsonlReader`, one task. Candidate: the range reader
over all associated ranks. The post-fix matrix passed **44/44** cases, including
ordered IDs/text, metadata, warnings where meaningful, exact no-loss/no-dup
sets, and byte-range coverage:

| Fixture family | Result |
|---|---|
| normal LF, trailing newline, missing final newline | PASS |
| empty file, one record, fewer records than requested parts | PASS |
| CRLF and UTF-8 Unicode content | PASS with UTF-8 DataTrove process mode |
| strongly variable/skewed sizes and a 1 MiB record | PASS; collapsed partitions explicit |
| malformed JSON line | PASS; warning-and-skip parity |
| explicit and implicit IDs | PASS; global physical line IDs |
| metadata, `default_metadata`, `text_key`, `id_key`, `add_file_path` | PASS |
| custom adapter | PASS where the DataTrove adapter contract is deterministic |
| `limit=0/1/5`, `skip`, `skip+limit` | PASS with a one-part plan, matching `BaseDiskReader` |

The source integration suite passed **17 tests** after the fix. The original
Windows run without UTF-8 mode exposed DataTrove's locale-codec behavior on the
Unicode fixture; rerunning with `-X utf8` produced the stated 44/44 result.

## 6. Real `LocalPipelineExecutor` proof

Fixture: deterministic 32 MiB uniform JSONL, 103,150 documents, four tasks and
four workers.

| Run | Documents per rank | Non-zero ranks | Total |
|---|---:|---:|---:|
| Native `JsonlReader`, 1 task | `[103150]` | 1 | 103150 |
| Native `JsonlReader`, 4 tasks | `[103150, 0, 0, 0]` | 1 | 103150 |
| `RangeJsonlReader`, 4 tasks | `[25827, 25807, 25773, 25743]` | 4 | 103150 |

Range byte counts were `[8388692, 8388940, 8388438, 8388661]`; they are
contiguous, non-overlapping, and cover the complete file. The independent
oracle count, ordered IDs/text, checksum, and parsed-document equality all
matched. This proves actual task parallelism; it is not the previous serial
helper loop.

## 7. Benchmark evidence

Runs used deterministic fixtures, one warm-up, repeated samples, median wall
time, real `LocalPipelineExecutor`, and workers 1/2/4. Planning and manifest
construction were recorded separately. Peak-RSS was not claimed because no
new heavy measurement dependency was introduced; `tracemalloc` regression
checks cover user-space allocation behavior.

| Profile / size | Native baseline | Range 1 worker | Range 2 workers | Range 4 workers |
|---|---:|---:|---:|---:|
| uniform / 32 MiB | 1.656 s | 1.416 s / 1.17x | 1.694 s / 0.98x | 1.546 s / 1.07x |
| uniform / 256 MiB | 8.990 s | 7.055 s / 1.27x | 4.760 s / 1.89x | 3.243 s / 2.77x |
| skewed / 32 MiB | 1.332 s | 1.255 s / 1.06x | 1.574 s / 0.85x | 1.491 s / 0.89x |
| skewed / 64 MiB | 1.924 s | 1.936 s / 0.99x | 1.940 s / 0.99x | 1.688 s / 1.14x |

Uniform 256 MiB throughput was 28.5 MiB/s for the native baseline versus
36.3/53.8/78.9 MiB/s for range workers 1/2/4. The 32 MiB results show the
startup and executor overhead clearly; skewed records reduce the advantage.
These are local cache/storage observations, not universal performance claims.

The benchmark also ran the fsspec transport variant. It preserved correctness
and was close to the binary local reader, so the result is about exact planning
and ownership plus parallel enablement, not a claim that mmap is the only
possible transport.

## 8. Memory-scaling result

The post-fix reader uses `seek(start)` plus bounded binary `readline()` and
parses one record at a time. Offset manifests scan 1 MiB blocks and retain only
integer offsets. Regression fixtures were larger than 4 MiB for record reading
and larger than 12 MiB for offset construction; both stayed below their
bounded-allocation assertions (`<2 MiB` and `<8 MiB`, respectively). The design
is O(max-record-size + bounded buffer + manifest), not O(worker-range-size).

The published 0.2.5 behavior is kept separate above: its whole-range read
peaked at approximately 16.94 MiB on the 4+ MiB fixture.

## 9. Files changed

Only focused Python integration, proof, test, and report files changed:

* `python/mmap_chunker/integrations/datatrove.py`
* `python/tests/test_datatrove_integration.py`
* `examples/datatrove_jsonl_range_reader.py`
* `examples/datatrove_fsspec_reader.py`
* `examples/datatrove_single_file_proof.py`
* `DATATROVE_ADOPTION_REPORT.md`

No Rust source, C header, ABI, dependency manifest, version, tag, release
workflow, `.env`, credential, or private configuration file was touched.

## 10. Validation

Completed before handoff:

* published PyPI baseline: reproduced H1/H2/H3/H4;
* post-fix parity: 44/44;
* source DataTrove integration tests: 17 passed;
* real LocalPipelineExecutor proof: native one-file sharding versus four
  range-owning ranks;
* uniform and skewed benchmark reports under `.audit-results/`;
* `python -m py_compile` for changed Python files;
* Rust `cargo fmt --check`, `cargo check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test` are required final checks even though Rust code is unchanged;
* the final staged diff check and full staged diff review are performed below,
  immediately before commit.

## 11. Conditional Issue #74 proposal (prepared, not posted)

> DataTrove `0.10.0` at main SHA `a649de79c14a550dc90f48a15c025f2dd3fd3b57`
> still assigns one local file to one task; with four tasks, three ranks are
> empty. We tested a small external `mmap-chunker-core` adapter for immutable,
> uncompressed local JSONL/NDJSON. It plans record-aligned ranges once and
> streams each worker range without materializing it. On a 32 MiB fixture,
> native 4-task execution processed `[103150,0,0,0]`, while the adapter
> processed `[25827,25807,25773,25743]`, exactly once each. On a 256 MiB
> uniform fixture, four workers measured 3.243 s versus 8.990 s for the native
> one-task baseline on this machine; smaller/skewed files were often neutral.
> Usage is approximately:
>
> ```python
> from mmap_chunker import plan_file
> from mmap_chunker.integrations.datatrove import RangeJsonlReader
> plan = plan_file("records.jsonl", parts=4)
> reader = RangeJsonlReader("records.jsonl", plan)
> ```
>
> The current PyPI `0.2.5` adapter has known `skip`/`limit` and whole-range
> memory defects; the repaired source is not released in this wave. Would
> maintainers prefer an optional adapter, external documentation, or a native
> DataTrove implementation? The supported scope is local/immutable/
> uncompressed/newline-delimited JSONL only.

This text is deliberately held for owner review. No upstream issue comment or
PR was opened.

## 12. Git deliverable and decision

The feature branch, commit SHA, draft PR URL, bundle path, bundle SHA-256, and
`git bundle verify` result are recorded in the final handoff after validation.
No force-push, main push, release, publish, or upstream action is permitted by
this wave.

**Final readiness:** the DataTrove adoption gap is real and the repaired source
is production-shaped, but the published package is not yet fixed. Therefore
the correct classification is
`OWN_ADAPTER_FIX_REQUIRED_BEFORE_UPSTREAM`, with the next gate being an owner
decision on releasing the focused adapter fix and then re-running the same
published-package proof.

**Single recommended next action:** owner-review the focused feature branch
and decide whether to authorize a small `0.2.6` release wave; do not contact
DataTrove until the repaired adapter is published and the evidence is rerun
against that release.
