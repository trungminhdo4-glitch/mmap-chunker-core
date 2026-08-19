# Python Wheel Distribution — Architecture Note

Status: decision record for the `feat/python-wheel-distribution` wave.
Starting main SHA: `af1d9d5bb05fae0a0f79072525b08e28aa67de2a` (includes merged PR #24).

## Goal

A user with no Rust toolchain must be able to run

```sh
pip install mmap-chunker-core
python -c "from mmap_chunker import plan_file; print(plan_file('records.jsonl', parts=8))"
```

with no Cargo, no separately downloaded CLI, no manual DLL/SO placement, no
subprocess planner, and no environment-variable loader hacks.

## Key architectural fact

The Rust shared library (`libmmap_chunker_core.{so,dylib}` / `mmap_chunker_core.dll`)
does **not** link against CPython. It exposes a stable C ABI (v1.3) that any
runtime can call. This means the Python package is a *pure-Python* package
from CPython's ABI perspective: it contains no CPython extension module, only
Python sources plus a bundled native shared library consumed through stdlib
`ctypes`.

Consequence: the wheel must be tagged `py3-none-<platform>`, **not**
`cp3xx-abi3-<platform>` and not one wheel per CPython minor version. One wheel
per platform serves every Python 3 version on that platform.

## Candidates evaluated

### A. stdlib ctypes + bundled existing cdylib  ✅ selected

- Runtime dependencies: none (ctypes is stdlib).
- Rust dependencies: none (reuses the existing cdylib).
- Python-version coupling: none — `py3-none-<platform>`.
- Wheels required: one per OS/arch (5 for the release matrix).
- Cross-platform: full, matching the existing release matrix.
- Reuses existing stable C ABI: yes, unchanged (ABI v1.3, cap bit 4).
- Wheel tagging: `py3-none-<platform>` via setuptools `bdist_wheel` override;
  platform tag verified per artifact.
- sdist behavior: source sdist includes Rust sources + `setup.py` that invokes
  `cargo` when the shared library is absent (documented; needs Cargo).
- Loading reliability: deterministic in-package path via `Path(__file__)`.
- CI complexity: one bounded workflow reusing the release matrix.
- Release duplication: build logic shared with `release.yml` contract; no new
  native pipeline.
- DataTrove friction: none — integrations import lazily.
- Maintenance: minimal; packaging metadata + one loader module.

### B. maturin + CFFI — rejected

maturin would add a build-time Rust dependency (`pyo3`-adjacent machinery) and
CFFI would add a **runtime** dependency (`cffi`). The project's core invariant
is zero runtime dependencies. CFFI does not materially simplify anything here:
the ABI is already C-visible and `ctypes` consumes it directly. No benefit
justifies the added runtime dependency.

### C. maturin + thin PyO3/abi3 wrapper — rejected

PyO3 would introduce a Rust dependency (`pyo3` with `abi3-py38`) and produce
`abi3` wheels that are still per-Python-major (py3-none is strictly simpler
and covers future CPython versions for free). It also risks a second native
entry point and a second ABI surface to keep in sync with the C ABI. The
mission explicitly says "Do not use PyO3 merely because the project is Rust."

### D. setuptools/custom PEP-517 platform-wheel build — selected in spirit

This is the mechanism behind option A: setuptools as the PEP-517 backend with
a `bdist_wheel` subclass that forces `root_is_pure = False` and the correct
platform tag. No separate backend needed.

### E. Other backends — not needed

No alternative backend is objectively simpler than stdlib ctypes + setuptools.

## Selected architecture

- Build backend: `setuptools.build_meta` (PEP 517).
- Native payload: the existing C-ABI cdylib, copied from the Cargo build
  output into the package `_native/` directory during wheel construction.
- Wheel tag: `py3-none-<platform>`.
  - Windows x86_64: `win_amd64`
  - Linux x86_64: `manylinux_2_17_x86_64` (GLIBC ceiling verified ≤ 2.17)
  - Linux aarch64: `manylinux_2_17_aarch64` (cross build, same contract)
  - macOS x86_64: `macosx_*_x86_64` (from the binary's deployment target)
  - macOS arm64: `macosx_*_arm64` (from the binary's deployment target)
- Loader: `mmap_chunker/_native.py` locates the library at
  `<package>/_native/<name>` via `Path(__file__)`, loads with `ctypes.CDLL`,
  and validates ABI version + `RECORD_PARTITIONING` capability.
- Public API: `mmap_chunker.plan_file` returning immutable `Plan`/`Range`
  dataclasses. No borrowed mmap memory is exposed.
- DataTrove: lazy optional integration under `mmap_chunker.integrations.datatrove`,
  enabled by the `[datatrove]` extra.
- Distribution name: `mmap-chunker-core` (available on PyPI; matches the crate
  name). Import namespace: `mmap_chunker` (stable regardless of dist name).

## Why py3-none-<platform> is correct (not mislabeled)

The wheel contains no `cpXXX`-tagged extension. The only native artifact is a
C-ABI shared library consumed via `ctypes`, which is CPython-version agnostic.
The `py3-none-<platform>` tag is the standard way to represent exactly that
fact (see packaging docs on pure wheels with platform-dependent payloads).
It is proven by clean-environment installs of the *same* wheel into Python
3.10, 3.12 and 3.14 on the same platform.

## Windows toolchain note

The release matrix builds Windows with `x86_64-pc-windows-msvc`. The wheel CI
job uses the same target so the DLL depends only on the MSVC/UCRT runtime.
The local proof machine only has the GNU toolchain; the GNU-built DLL was
verified to import only system DLLs (KERNEL32, msvcrt, ntdll, WS2_32,
USERENV, UCRT API sets) — no MinGW runtime DLLs — so it is also portable.
CI Windows wheels use MSVC to match the release contract exactly.

## Non-goals

- No PyPI publication in this wave (artifacts uploaded to Actions only).
- No new Rust API, no C ABI change, no ABI version bump.
- No runtime Cargo dependency, no runtime Python dependency.