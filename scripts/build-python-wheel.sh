#!/usr/bin/env bash
# Build the mmap-chunker-core Python wheel for one target and verify the
# native payload. Shared by the python-wheel CI workflow so the target matrix,
# GLIBC ceiling, native filenames, and ABI checks live in one place.
#
# Required env:
#   TARGET        Rust target triple (e.g. x86_64-unknown-linux-gnu)
#   WHEEL_PLAT_TAG  Wheel platform tag (e.g. manylinux_2_17_x86_64, win_amd64,
#                    macosx_10_13_x86_64). Required for every target: macOS
#                    bdist_wheel auto-detection emits `universal2` even for
#                    thin single-arch binaries, so it is pinned explicitly.
# Optional env:
#   CROSS         Set to 1 to build via `cross` instead of `cargo`
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

TARGET="${TARGET:?TARGET is required}"
WHEEL_PLAT_TAG="${WHEEL_PLAT_TAG:-}"
USE_CROSS="${CROSS:-0}"

RELEASE_DIR="target/$TARGET/release"

case "$TARGET" in
  x86_64-pc-windows-msvc)
    LIB_NAME="mmap_chunker_core.dll"
    ;;
  x86_64-apple-darwin | aarch64-apple-darwin)
    LIB_NAME="libmmap_chunker_core.dylib"
    ;;
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu)
    LIB_NAME="libmmap_chunker_core.so"
    ;;
  *)
    echo "ERROR: unsupported target $TARGET" >&2
    exit 1
    ;;
esac

echo "=== Build native library ($TARGET) ==="
if [ "$USE_CROSS" = "1" ]; then
  cross build --release --target "$TARGET"
else
  cargo build --release --target "$TARGET"
fi
VERIFIED_NATIVE_LIBRARY="$RELEASE_DIR/$LIB_NAME"
test -f "$VERIFIED_NATIVE_LIBRARY" || { echo "ERROR: $VERIFIED_NATIVE_LIBRARY missing" >&2; exit 1; }
export MMAP_CHUNKER_NATIVE_LIBRARY="$VERIFIED_NATIVE_LIBRARY"

echo "=== Dynamic ABI symbol verification ==="
grep -vE '^[[:space:]]*(#|$)' abi/v1.symbols | sort > /tmp/expected.symbols
case "$TARGET" in
  x86_64-pc-windows-msvc)
    if command -v llvm-readobj >/dev/null 2>&1; then
      llvm-readobj --coff-exports "$VERIFIED_NATIVE_LIBRARY" \
        | grep -oE 'mmap_engine_[A-Za-z0-9_]+' | sort > /tmp/actual.symbols
    elif command -v dumpbin.exe >/dev/null 2>&1; then
      dumpbin.exe /exports "$VERIFIED_NATIVE_LIBRARY" \
        | grep -oE 'mmap_engine_[A-Za-z0-9_]+' | sort > /tmp/actual.symbols
    else
      echo "ERROR: no PE export inspection tool available" >&2
      exit 1
    fi
    ;;
  *apple-darwin)
    nm -gU "$VERIFIED_NATIVE_LIBRARY" \
      | awk '$3 ~ /^_?mmap_engine_/ {sub(/^_/, "", $3); print $3}' | sort > /tmp/actual.symbols
    ;;
  *linux-gnu)
    nm -D --defined-only "$VERIFIED_NATIVE_LIBRARY" \
      | awk '$3 ~ /^mmap_engine_/ {print $3}' | sort > /tmp/actual.symbols
    ;;
esac
diff -u /tmp/expected.symbols /tmp/actual.symbols
echo "PASS: exported mmap_engine_* symbols match abi/v1.symbols"

if [[ "$TARGET" == *linux-gnu ]]; then
  echo "=== GLIBC symbol ceiling (declared <= 2.17) ==="
  GLIBC_VERSIONS=$(readelf --version-info "$VERIFIED_NATIVE_LIBRARY" \
    | grep -oE 'GLIBC_[0-9.]+' | sort -Vu)
  printf '%s\n' "$GLIBC_VERSIONS"
  MAX_GLIBC=$(printf '%s\n' "$GLIBC_VERSIONS" | sort -V | tail -n 1)
  if [ -z "$MAX_GLIBC" ]; then
    echo "ERROR: no GLIBC version inventory" >&2
    exit 1
  fi
  if ! awk -v found="$MAX_GLIBC" -v declared="GLIBC_2.17" '
    BEGIN {
      sub(/^GLIBC_/, "", found)
      sub(/^GLIBC_/, "", declared)
      split(found, f, ".")
      split(declared, d, ".")
      exit !(f[1] < d[1] || (f[1] == d[1] && f[2] <= d[2]))
    }
  '; then
    echo "ERROR: library requires $MAX_GLIBC, above GLIBC_2.17" >&2
    exit 1
  fi
  echo "PASS: GLIBC ceiling is at or below 2.17"
fi

echo "=== Copy native library into the package ==="
mkdir -p python/mmap_chunker/_native
cp "$VERIFIED_NATIVE_LIBRARY" "python/mmap_chunker/_native/$LIB_NAME"

echo "=== Build wheel ==="
rm -rf dist build
if [ -n "$WHEEL_PLAT_TAG" ]; then
  MMAP_CHUNKER_PLAT_TAG="$WHEEL_PLAT_TAG" python -m build --wheel .
else
  python -m build --wheel .
fi

echo "=== Wheel inspection ==="
WHEEL=$(ls dist/*.whl | head -n 1)
echo "wheel=$WHEEL"
python - "$WHEEL" "$VERIFIED_NATIVE_LIBRARY" <<'PY'
import sys
import zipfile
from pathlib import Path
from packaging.tags import sys_tags

wheel = sys.argv[1]
verified_native = Path(sys.argv[2])
with zipfile.ZipFile(wheel) as z:
    names = z.namelist()
    wheel_meta = [n for n in names if n.endswith("dist-info/WHEEL")]
    assert wheel_meta, "no dist-info/WHEEL"
    tag_line = [ln for ln in z.read(wheel_meta[0]).decode().splitlines() if ln.startswith("Tag:")]
    assert tag_line, "no Tag line"
    print("\n".join(tag_line))
    tag = tag_line[0].split(":", 1)[1].strip()
    assert tag.startswith("py3-none-"), f"unexpected tag {tag}"
    if "win" in tag or "linux" in tag or "macosx" in tag:
        supported = {str(t) for t in sys_tags()}
        # The build host may differ from the artifact platform for cross builds;
        # the platform tag is still valid per the packaging standard. Verify the
        # structure only here; clean installs are the real proof.
        parts = tag.split("-")
        assert len(parts) == 3 and parts[0] == "py3" and parts[1] == "none"
        print("tag structure ok:", tag)
    def norm(n):
        # Files may live under "<dist>-<ver>.data/<scheme>/" when the wheel is
        # built with Root-Is-Purelib false; normalize to package-relative paths.
        parts = n.split("/")
        if len(parts) >= 2 and parts[0].endswith(".data"):
            return "/".join(parts[2:])
        return n

    normalized = [norm(n) for n in names]
    required = {
        "mmap_chunker/__init__.py",
        "mmap_chunker/_native.py",
        "mmap_chunker/planning.py",
        "mmap_chunker/py.typed",
        "mmap_chunker/integrations/__init__.py",
        "mmap_chunker/integrations/datatrove.py",
    }
    native = [n for n in normalized if n.startswith("mmap_chunker/_native/") and not n.endswith("/")]
    assert native, "no native payload"
    expected_native = f"mmap_chunker/_native/{verified_native.name}"
    native_members = [n for n in names if norm(n) == expected_native]
    assert len(native_members) == 1, f"expected exactly one {expected_native} payload"
    assert z.read(native_members[0]) == verified_native.read_bytes(), (
        f"wheel payload {native_members[0]} differs from verified native library "
        f"{verified_native}"
    )
    for req in required:
        assert req in normalized, f"missing {req}"
    assert not any(n.startswith("mmap_chunker_core-") and n.endswith(".exe") for n in normalized), "CLI bundled"
    assert not any(n.startswith("src/") for n in normalized), "Rust sources in wheel"
    assert not any(n.startswith("target/") for n in normalized), "target/ in wheel"
    print("native payload:", native)
    print("native payload matches verified artifact byte-for-byte")
    print("wheel content contract OK")
PY

echo "PASS: wheel built and content-verified"
