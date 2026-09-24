# Release Process

## Version Domains

Three independent version domains:

| Domain        | Current            | Controls                                  |
|---------------|--------------------|-------------------------------------------|
| Crate SemVer  | 0.2.2              | crates.io package, GitHub tag, Release    |
| C ABI         | 1.6 (`0x00010006`) | Additive C API capability evolution       |
| Rust MSRV     | 1.77               | Minimum Supported Rust Version            |

Crate SemVer and C ABI version evolve independently.

Sources: `Cargo.toml` `version = "0.2.2"` (HEAD + worktree identisch,
nur `exclude`-Zeile unterscheidet sich); `src/ffi.rs:26`
`pub const ABI_VERSION: u32 = 0x0001_0006`; `AGENTS.md`
"Public C ABI (14 functions, ABI v1.6)".

## Known Drift — Tags vs. Cargo (KEIN Version-Bump in dieser Änderung)

Git-Tags existieren bis `v0.2.6` (`v0.1.0` … `v0.2.6` per
`git tag --list`; `v0.2.3` `db12a56` 2026-08-14 … `v0.2.6` `85d3a32`
2026-08-21), während `Cargo.toml` im HEAD (`19a628d`,
Branch `feat/prebuilt-cli-distribution`) und im Worktree weiterhin
`0.2.2` meldet (`v0.2.6:Cargo.toml` meldet `0.2.6`).
Der Worktree ist zusätzlich dirty (`git status --short`: ~30× `M` plus
`?? integrations/`, `?? tools/`, `?? src/{framing,source,manifest,index,json}.rs`,
`?? native_io/{plan_manifest,record_index}.py`,
`?? tests/test_{plan_manifest,record_index}.py`).

Das heißt: Tag-Drift + dirty Worktree — die Pre-Publish-Bedingungen
"Tag match" und "Clean worktree" sind aktuell NICHT erfüllt.
Diese Datei dokumentiert den Drift nur; es erfolgt hier bewusst
KEIN Version-Bump, KEIN Re-Tag, KEIN Commit (Verbot im Scope).
Owner-Entscheide ausstehend: (1) Version-Bump ja/nein — `Cargo.toml`
auf `0.2.3+`/`0.2.6+`/nächste Minor anheben oder Tags als überholt
markieren; (2) Commit ja/nein — pending Module (s. CHANGELOG
`[Unreleased]` Pending-Notiz) committen oder weiter uncommitted lassen.

## Pre-Publish Checklist

Before `cargo publish` for any version:

1. **Tag match**: `git rev-parse HEAD` must equal `git rev-parse <tag>^{}`
   - Never publish from an untagged, dirty, or non-tag commit.
2. **Clean worktree**: `git status --porcelain` must be empty
3. **Cargo.toml version** must match the tag exactly
4. **All gates pass**: `cargo fmt --check && cargo check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release`
5. **Package verification**: `cargo package` succeeds, version is correct, no private files
6. **CHANGELOG**: Release section exists with correct date

## Automated Release Assets

Pushing a `vX.Y.Z` tag triggers `.github/workflows/release.yml`:

- Validates tag == `Cargo.toml` version
- Creates a draft GitHub Release
- Matrix-builds 5 platforms (Linux x86_64/aarch64, macOS x86_64/arm64, Windows x86_64)
- Uploads 5 native-library archives (header + dynamic + static library) and 5
  standalone CLI archives, each with a sha256 checksum sidecar
- The draft release therefore contains 20 assets: 10 native-library assets and
  10 standalone CLI assets

The draft is published manually after review. crates.io publish remains a separate manual step (see Publish Order below).

## Publish Order

1. Create annotated tag on the exact commit:
   ```
   git tag -a v<version> -m "v<version>"
   ```
2. Push tag: `git push origin v<version>`
3. Create GitHub Release from that tag
4. Publish from pristine tag worktree (NEVER from main):
   ```
   git worktree add ../mmap-chunker-core-v<version> v<version>
   cd ../mmap-chunker-core-v<version>

   git status --porcelain
   git describe --exact-match --tags HEAD
   cargo publish --dry-run
   cargo publish
   ```
5. Verify crates.io and docs.rs

## Anti-Patterns

- **NEVER** publish when `Cargo.toml version` matches an existing tag but HEAD differs
- **NEVER** publish from a dirty working tree
- **NEVER** move an existing tag
