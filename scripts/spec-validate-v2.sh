#!/usr/bin/env bash
# spec-validate-v2.sh — v2 shape oracle against a throwaway ocserve.
#
# Boots the release (or debug) ocserve with OCSERVE_V2=1 under a fixture
# HOME on a throwaway port, runs spec-validate.py against the 2.0.28
# contract, then kills the server. Exit 0 = 0 violations; the validator
# itself enforces the D-V2-UNION allowlist (named divergences only).
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${SPEC_V2_PORT:-4955}"
BIN="${OCSERVE_BIN:-target/release/ocserve}"
[ -x "$BIN" ] || BIN="target/debug/ocserve"
[ -x "$BIN" ] || { echo "spec-validate-v2: no ocserve binary" >&2; exit 1; }

HOME2="$(mktemp -d "${TMPDIR:-/var/tmp}/ocserve-specv2.XXXXXX")"
mkdir -p "$HOME2/.local/share/ocserve" "$HOME2/.config/ocserve"
printf '{"$schema":"x","mcp":{},"plugin":[]}' >"$HOME2/.config/ocserve/opencode.json"
cleanup() {
  pkill -f "ocserve serve --port $PORT" 2>/dev/null || true
  rm -rf "$HOME2"
}
trap cleanup EXIT

pkill -f "ocserve serve --port $PORT" 2>/dev/null || true
sleep 1
setsid env HOME="$HOME2" OCSERVE_V2=1 "$BIN" serve --port "$PORT" --hostname 127.0.0.1 >"$HOME2/serve.log" 2>&1 &
for _ in $(seq 1 40); do
  curl -s --max-time 1 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -s --max-time 2 "http://127.0.0.1:$PORT/global/health" >/dev/null \
  || { echo "spec-validate-v2: server did not start"; tail -5 "$HOME2/serve.log"; exit 1; }

python3 bench/openapi/spec-validate.py "http://127.0.0.1:$PORT" \
  --spec crates/ocserve-core/assets/openapi/2.0.28.json
