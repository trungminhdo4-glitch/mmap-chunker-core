#!/usr/bin/env bash
# Assert the exact PyPI distribution inventory for one release.
#
# The v0.2.5 publication set is exactly: five platform wheels plus the sdist.
# This rejects duplicates, unexpected files, wrong versions, wrong wheel tags,
# and missing platforms. It must pass before any upload to PyPI.
#
# Usage: assert-pypi-distributions.sh <dist-dir> <version>
set -euo pipefail

DIST_DIR="${1:?dist dir is required}"
VERSION="${2:?version is required}"
V="${VERSION//./\\.}"

mapfile -t FILES < <(find "$DIST_DIR" -maxdepth 1 -type f -printf '%f\n' | sort)
if [ "${#FILES[@]}" -ne 6 ]; then
  echo "ERROR: expected exactly 6 distributions, found ${#FILES[@]}" >&2
  printf '  %s\n' "${FILES[@]:-}" >&2
  exit 1
fi

# Duplicate check (sorted input makes adjacent duplicates easy to spot).
for i in "${!FILES[@]}"; do
  if [ "$i" -gt 0 ] && [ "${FILES[$i]}" = "${FILES[$((i - 1))]}" ]; then
    echo "ERROR: duplicate distribution file ${FILES[$i]}" >&2
    exit 1
  fi
done

declare -A SEEN
for f in "${FILES[@]}"; do
  case "$f" in
    "mmap_chunker_core-${V}-py3-none-manylinux_2_17_x86_64.whl")
      SEEN[lx86]="$f" ;;
    "mmap_chunker_core-${V}-py3-none-manylinux_2_17_aarch64.whl")
      SEEN[laarch64]="$f" ;;
    mmap_chunker_core-${V}-py3-none-macosx_*_x86_64.whl)
      SEEN[macx86]="$f" ;;
    mmap_chunker_core-${V}-py3-none-macosx_*_arm64.whl)
      SEEN[macarm64]="$f" ;;
    "mmap_chunker_core-${V}-py3-none-win_amd64.whl")
      SEEN[win]="$f" ;;
    "mmap_chunker_core-${V}.tar.gz")
      SEEN[sdist]="$f" ;;
    *)
      echo "ERROR: unexpected distribution file $f" >&2
      exit 1
      ;;
  esac
done

MISSING=()
for k in lx86 laarch64 macx86 macarm64 win sdist; do
  if [ -z "${SEEN[$k]:-}" ]; then
    MISSING+=("$k")
  fi
done
if [ "${#MISSING[@]}" -ne 0 ]; then
  echo "ERROR: missing expected platforms: ${MISSING[*]}" >&2
  exit 1
fi

echo "PASS: exact PyPI distribution inventory for v$VERSION verified"
for k in lx86 laarch64 macx86 macarm64 win sdist; do
  echo "  $k -> ${SEEN[$k]}"
done
