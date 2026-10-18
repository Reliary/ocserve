#!/usr/bin/env bash
# acquire-freeze — stage the frozen upstream (opencode 1.18.31) for the
# differential gates (capdiff, pair) on a machine that has no host opencode.
#
# The freeze binary is NEVER committed (185 MB). It is fetched from npm at the
# pinned version — the same mechanism bench/parity/Dockerfile.upstream-npm uses
# — resolved to the platform package, then copied to bench/parity/vendor/opencode
# where pair-check.sh / capdiff-check.sh already look for it.
#
# Idempotent: if a correct vendor binary already exists it is left alone.
# Exit: 0 staged/ok · 2 infra (no npm, download failed, wrong version).
set -euo pipefail
cd "$(dirname "$0")/.."

VENDOR="bench/parity/vendor/opencode"
# Positional version wins (bump.sh v1 passes it), then env, then the pinned
# default. Before this, a positional arg was silently IGNORED and the pinned
# default staged regardless — `acquire-freeze.sh 1.19.3` fetched 1.18.31.
WANT="${1:-${OCSERVE_FREEZE_VERSION:-1.18.31}}"

if [ -x "$VENDOR" ] && [ "$("$VENDOR" --version 2>/dev/null | head -1)" = "$WANT" ]; then
  echo "acquire-freeze: $VENDOR already at $WANT"
  exit 0
fi

command -v npm >/dev/null 2>&1 || { echo "acquire-freeze: npm not found" >&2; exit 2; }

# A host opencode of the exact version is the cheapest source (dev boxes).
host="$(command -v opencode 2>/dev/null || true)"
if [ -n "$host" ] && [ "$("$host" --version 2>/dev/null | head -1)" = "$WANT" ]; then
  mkdir -p "$(dirname "$VENDOR")"
  cp "$host" "$VENDOR" && chmod +x "$VENDOR"
  echo "acquire-freeze: staged from host $host"
  exit 0
fi

echo "acquire-freeze: installing opencode-ai@${WANT} from npm"
prefix="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-freeze.XXXXXX")"
npm install -g --prefix "$prefix" "opencode-ai@${WANT}" >/dev/null 2>&1 \
  || { echo "acquire-freeze: npm install failed" >&2; rm -rf "$prefix"; exit 2; }

src="$prefix/bin/opencode"
[ -x "$src" ] || src="$(find "$prefix" -type f -name opencode -perm -u+x | head -1)"
[ -n "$src" ] && [ -x "$src" ] || { echo "acquire-freeze: opencode binary not found in npm prefix" >&2; rm -rf "$prefix"; exit 2; }

got="$("$src" --version 2>/dev/null | head -1)"
[ "$got" = "$WANT" ] || { echo "acquire-freeze: got '$got', want '$WANT'" >&2; rm -rf "$prefix"; exit 2; }

mkdir -p "$(dirname "$VENDOR")"
cp "$src" "$VENDOR" && chmod +x "$VENDOR"
rm -rf "$prefix"
echo "acquire-freeze: staged $VENDOR ($WANT)"
