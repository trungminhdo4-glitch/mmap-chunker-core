# Release Process

## Version Domains

Three independent version domains:

| Domain        | Current  | Controls                                  |
|---------------|----------|-------------------------------------------|
| Crate SemVer  | 0.2.6    | crates.io package, PyPI distribution, GitHub tag, Release |
| C ABI         | 1.3      | Additive C API capability evolution       |
| Rust MSRV     | 1.77     | Minimum Supported Rust Version            |

The Rust crate and the Python distribution share one version: the Python
package version is derived from `Cargo.toml` (`setup.py` reads it directly).
`python/mmap_chunker.__version__` resolves from the installed metadata, which
inherits the same Cargo-derived version. Crate SemVer, C ABI version, and Rust
MSRV evolve independently.

## Release Artifact Inventory (v0.2.6)

The release produces one verified artifact set from the release commit:

- **GitHub Release** (draft, manually published): native SDK archives and
  standalone CLI archives — 5 native-library archives (header + dynamic +
  static library + licenses) and 5 standalone CLI archives, each with a SHA-256
  sidecar. Python wheels are **not** duplicated on GitHub Releases; PyPI is
  their distribution channel.
- **PyPI**: 5 platform wheels + 1 sdist:
  - `mmap_chunker_core-0.2.6-py3-none-manylinux_2_17_x86_64.whl`
  - `mmap_chunker_core-0.2.6-py3-none-manylinux_2_17_aarch64.whl`
  - `mmap_chunker_core-0.2.6-py3-none-macosx_*_x86_64.whl`
  - `mmap_chunker_core-0.2.6-py3-none-macosx_*_arm64.whl`
  - `mmap_chunker_core-0.2.6-py3-none-win_amd64.whl`
  - `mmap_chunker_core-0.2.6.tar.gz`
- **crates.io**: `mmap-chunker-core-0.2.6` crate (from `cargo package`).

## Trusted Publishing (OIDC) Architecture

Both registries are published with OpenID Connect Trusted Publishing — no API
tokens, no long-lived credentials, no password secrets.

- **PyPI**: `pypa/gh-action-pypi-publish` (pinned by SHA) in job
  `publish-pypi` of `.github/workflows/release.yml`, scoped to the protected
  GitHub environment `pypi` with `id-token: write` at job scope only. It
  consumes the already-built and already-verified wheels + sdist, asserts the
  exact distribution inventory, and then uploads.
- **crates.io**: `rust-lang/crates-io-auth-action` (pinned by SHA) exchanges
  the GitHub OIDC token for a temporary crates.io access token in job
  `publish-crate`, scoped to the protected GitHub environment `crates-io`.
  `cargo publish` runs with that temporary token (auto-revoked at job end).
- The crates.io trusted-publisher configuration for
  `trungminhdo4-glitch/mmap-chunker-core` must reference repository
  `trungminhdo4-glitch/mmap-chunker-core`, workflow `release.yml`, and
  environment `crates-io`.

External setup that is not part of this repository:

- PyPI: add a trusted publisher for project `mmap-chunker-core` (owner
  `trungminhdo4-glitch`, repo `mmap-chunker-core`, workflow `release.yml`,
  environment `pypi`).
- crates.io: add a trusted publisher for the crate (repo
  `trungminhdo4-glitch/mmap-chunker-core`, workflow `release.yml`, environment
  `crates-io`). Optionally enable trusted-publishing-only mode on the crate.
- GitHub: create protected environments `pypi` and `crates-io` and configure a
  required reviewer (owner approval) on each so an owner gate immediately
  precedes irreversible publication.

## Trigger Model

- **Tag push** (`push` on a `v*.*.*` tag) runs the full release path:
  prepare → build → verify → package contract → owner approval (environments)
  → publish PyPI + crates.io → create draft GitHub Release. `release.yml`
  validates that the tag equals the `Cargo.toml` version before anything else.
- **`workflow_dispatch`** is the strongest non-publishing dry run: it builds
  and verifies the native SDK archives, CLI archives, Python wheels, and Python
  sdist, uploads everything as Actions artifacts, and validates the combined
  inventory. The publish jobs are strictly gated on `push` tag events, so a
  dry run can never publish.
- `pull_request_target` is never used; PRs never reach a publisher job.

## Automated Release Assets

Pushing a `vX.Y.Z` tag triggers `.github/workflows/release.yml`:

- Validates tag == `Cargo.toml` version
- Matrix-builds 5 platforms (Linux x86_64/aarch64, macOS x86_64/arm64,
  Windows x86_64) for native SDK and standalone CLI archives
- Calls `.github/workflows/python-wheel.yml` (reusable) to build + verify the
  5 platform wheels and the sdist
- Creates a draft GitHub Release with 20 assets (10 native-library + 10
  standalone CLI, each with a sha256 sidecar)
- Publishes the wheel set + sdist to PyPI via Trusted Publishing
- Publishes the crate to crates.io via Trusted Publishing

The draft is published manually after review. The crate and PyPI publishes are
the irreversible steps and are guarded by protected environment approvals.

## Publish Order

Conceptual sequence enforced by the workflow dependency graph:

1. Create annotated tag on the exact release commit:
   ```
   git tag -a v<version> -m "v<version>"
   ```
2. Push tag: `git push origin v<version>`
3. `release.yml` builds and verifies every artifact from the tag commit.
4. Owner approves environment gates (`pypi`, `crates-io`).
5. `publish-crate` publishes the crate to crates.io (OIDC).
6. `publish-pypi` publishes the wheels + sdist to PyPI (OIDC).
7. `publish-release` creates the draft GitHub Release (last, so a registry
   failure blocks the GitHub Release step).
8. Verify crates.io, docs.rs, and PyPI.
9. Publish the draft GitHub Release manually.

The `publish-release` job depends on both `publish-crate` and `publish-pypi`
succeeding, so one ecosystem cannot bypass a failure in the other.

## Pre-Publish Checklist

Before any release:

1. **Tag match**: `git rev-parse HEAD` must equal `git rev-parse <tag>^{}`
   - Never publish from an untagged, dirty, or non-tag commit.
2. **Clean worktree**: `git status --porcelain` must be empty
3. **Cargo.toml version** must match the tag exactly
4. **All gates pass**: `cargo fmt --check && cargo check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release`
5. **Package verification**: `cargo package` succeeds, version is correct, no private files
6. **CHANGELOG**: Release section exists with correct date
7. **Python proof**: wheel matrix (5 platforms), python-version matrix
   (3.10/3.12/3.14 same-wheel), DataTrove smoke, sdist rebuild — all green
8. **Version provenance**: `cargo metadata`, PyPI METADATA, sdist PKG-INFO,
   `mmap_chunker.__version__`, CLI `--version`, tag, and GitHub Release name all
   resolve to the same version

## Manual fallback (not recommended)

If OIDC is ever unavailable, publishing can be done manually from a pristine
tag worktree (NEVER from main):

```
git worktree add ../mmap-chunker-core-v<version> v<version>
cd ../mmap-chunker-core-v<version>

git status --porcelain
git describe --exact-match --tags HEAD
cargo publish --dry-run
cargo publish
python -m build --sdist .
python -m twine upload dist/*   # requires a PyPI token
```

This is the fallback only; the standard path uses Trusted Publishing and does
not require any long-lived credential.

## Anti-Patterns

- **NEVER** publish when `Cargo.toml version` matches an existing tag but HEAD differs
- **NEVER** publish from a dirty working tree
- **NEVER** move an existing tag
- **NEVER** add PyPI/crates.io API tokens as repository or environment secrets
  while Trusted Publishing is configured
- **NEVER** widen `id-token: write` beyond the two publish jobs
