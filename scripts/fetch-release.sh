#!/usr/bin/env bash
# fetch-release.sh — get a released ocserve binary from the CI-built artifacts.
#
# The release is the product (release.yml): this script downloads a tagged
# tarball + SHA256SUMS, verifies the checksum, and prints the path to the
# extracted binary. It performs no build and needs no Rust toolchain, so a host
# that only runs the server never compiles 11 crates to update.
#
# Usage:
#   scripts/fetch-release.sh [TAG] [--dest DIR] [--platform linux-x86_64]
#   scripts/fetch-release.sh --selftest
#
# Defaults: TAG = latest release, platform = linux-x86_64 (override for
# linux-aarch64). --dest defaults to the OS temp dir ($TMPDIR).
#
# Refuses on: checksum mismatch, missing asset, unpinned download tool.
# --selftest proves the checksum gate bites (corrupt the tarball, expect fail).
set -euo pipefail

REPO_SLUG=${OCSERVE_REPO_SLUG:-Reliary/ocserve}
TAG="" DEST="" PLATFORM="linux-x86_64" SELFTEST=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dest)     DEST=$2; shift ;;
    --platform) PLATFORM=$2; shift ;;
    --selftest) SELFTEST=1 ;;
    -h|--help)  sed -n '2,15p' "$0"; exit 0 ;;
    -*)         echo "fetch-release.sh: unknown flag: $1" >&2; exit 2 ;;
    *)          TAG=$1 ;;
  esac
  shift
done

say() { printf '%s\n' "$*" >&2; }

# --- checksum verification (the whole point of this script) ------------------
# verify_sum FILE SUMS  -> 0 when the file's sha256 matches the line in SUMS.
verify_sum() {
  local file=$1 sums=$2 base want got
  base=$(basename "$file")
  # CI writes bare names; a local `sha256sum` writes full paths. Match on the
  # basename of the recorded path so both forms verify.
  want=$(awk -v f="$base" '{ n=$2; sub(/.*\//, "", n); if (n == f) { print $1; exit } }' "$sums")
  [ -n "$want" ] || { say "FAIL: no checksum entry for $base"; return 1; }
  got=$(sha256sum "$file" | awk '{print $1}')
  [ "$want" = "$got" ] || { say "FAIL: checksum mismatch for $base (want $want, got $got)"; return 1; }
  return 0
}

if [ "$SELFTEST" = 1 ]; then
  d=$(mktemp -d); trap 'rm -rf "$d"' EXIT
  printf 'payload' > "$d/a.tar.gz"
  sha256sum "$d/a.tar.gz" > "$d/SHA256SUMS"
  verify_sum "$d/a.tar.gz" "$d/SHA256SUMS" || { say "FAIL: selftest: a good checksum was rejected"; exit 1; }
  printf 'tampered' > "$d/a.tar.gz"
  if verify_sum "$d/a.tar.gz" "$d/SHA256SUMS" 2>/dev/null; then
    say "FAIL: selftest: a corrupted tarball passed verification"; exit 1
  fi
  say "ok: fetch-release selftest (good accepted, corrupted rejected)"
  exit 0
fi

command -v curl >/dev/null 2>&1 || { say "FAIL: curl is required"; exit 1; }

if [ -z "$TAG" ]; then
  TAG=$(curl -fsSL "https://api.github.com/repos/$REPO_SLUG/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$TAG" ] || { say "FAIL: could not resolve the latest release tag"; exit 1; }
  say "latest release: $TAG"
fi

case "$PLATFORM" in
  linux-x86_64|linux-aarch64) ;;
  *) say "FAIL: unsupported --platform (linux-x86_64|linux-aarch64)"; exit 1 ;;
esac

VER=${TAG#v}
NAME="ocserve-v${VER}-${PLATFORM}"
BASE="https://github.com/$REPO_SLUG/releases/download/$TAG"
DEST=${DEST:-${TMPDIR:-/tmp}}
mkdir -p "$DEST"

for f in "$NAME.tar.gz" SHA256SUMS; do
  say "fetching $f"
  curl -fsSL -o "$DEST/$f" "$BASE/$f" || { say "FAIL: download failed: $BASE/$f"; exit 1; }
done

verify_sum "$DEST/$NAME.tar.gz" "$DEST/SHA256SUMS"
say "checksum ok"

tar -C "$DEST" -xzf "$DEST/$NAME.tar.gz"
BIN="$DEST/$NAME/ocserve"
[ -x "$BIN" ] || { say "FAIL: tarball did not contain an executable ocserve"; exit 1; }
say "binary: $BIN"
printf '%s\n' "$BIN"
