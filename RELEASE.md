# Release Process

## Version Domains

Three independent version domains:

| Domain        | Current  | Controls                                  |
|---------------|----------|-------------------------------------------|
| Crate SemVer  | 0.2.1    | crates.io package, GitHub tag, Release    |
| C ABI         | 1.3      | Additive C API capability evolution       |
| Rust MSRV     | 1.77     | Minimum Supported Rust Version            |

Crate SemVer and C ABI version evolve independently.

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
- Uploads per-platform archives (header + dynamic + static library) + sha256 checksums

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
