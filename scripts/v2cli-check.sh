#!/usr/bin/env bash
# Live v2 CLIENT acceptance (GAPS-PLAN Tier 1.2) — the Phase A arbitration.
#
# ocserve's v2 surface was proven by pair-v2-check driving the 2.0.28 CLI **as a
# server**; no live v2 CLIENT had ever booted against ocserve. This runs the REAL
# `@opencode/cli@2.0.28` as a CLIENT (`--server <ocserve>`) through the commands
# that reach the v2 connect + read surface, captures every route it calls, and
# asserts each is served JSON (not the SPA HTML crash class). Mirrors
# scripts/tui-check.sh for v1.
#
# Scope note: only the CLI subcommands proven non-interactive are driven
# (`session list`, `models`); the `api`/`agent`/`mcp` subcommands block or
# mis-parse a positional directory and are excluded. The point is the CLIENT's
# real route traffic, not exhaustive CLI coverage.
#
# Exit 0 when no v2-client-reachable route is served HTML. Manual/nightly.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${OCSERVE_V2UI_PORT:-5121}"
DATA="$(mktemp -d /tmp/ocserve-v2cli.XXXXXX)"
LOG="$DATA/serve.log"
OCSERVE_BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
V2CLI="${V2_CLI:-/var/tmp/ocserve-scratch/ocli/node_modules/@opencode/cli-linux-x64/bin/opencode}"

cleanup() {
  [ -n "${PID:-}" ] && kill -- -"$PID" 2>/dev/null
  pkill -f "ocserve serve --port $PORT" 2>/dev/null
  rm -rf "$DATA"
}
trap cleanup EXIT

[ -x "$OCSERVE_BIN" ] || { echo "v2cli-check: build release first ($OCSERVE_BIN missing)" >&2; exit 2; }
[ -x "$V2CLI" ] || { echo "v2cli-check: 2.0.28 CLI missing at $V2CLI" >&2; exit 2; }

FH="$DATA/fixture-home"
cp -r "$PWD/testdata/fixture" "$FH"

setsid env HOME="$FH" OCSERVE_DATA_DIR="$DATA" OCSERVE_LEGACY_SYNC=0 \
  OCSERVE_V2=1 OCSERVE_REQ_LOG=1 OPENCODE_DISABLE_MODELS_FETCH=1 RUST_LOG=info "$OCSERVE_BIN" serve \
  --port "$PORT" >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 80); do
  curl -s --max-time 1 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  curl -s --max-time 1 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break
  kill -0 "$PID" 2>/dev/null || { echo "v2cli-check: serve died"; cat "$LOG"; exit 2; }
  sleep 0.25
done

# Seed a session so the client's list path exercises real data.
curl -s -X POST -H 'content-type: application/json' \
  -d "{\"title\":\"v2cli seed\",\"location\":{\"directory\":\"$DATA\"}}" \
  "http://127.0.0.1:$PORT/api/session" >/dev/null

echo "== driving the real 2.0.28 CLI as a CLIENT against ocserve =="
run_client() {
  local label="$1"; shift
  timeout 30 "$V2CLI" "$@" --server "http://127.0.0.1:$PORT" </dev/null \
    >"$DATA/cmd_$label.out" 2>&1 || true
}
run_client session_list session list
run_client models models

# The client must have actually connected and listed the seeded session.
if grep -q "v2cli seed" "$DATA/cmd_session_list.out"; then
  echo "ok (client listed the seeded session through the v2 API)"
else
  echo "FAIL: the 2.0.28 client did not list the seeded session" >&2
  tail -15 "$DATA/cmd_session_list.out" >&2
  exit 1
fi

# Any route the client hit must be JSON, never HTML.
paths=$(grep -oE 'req [A-Z]+ [^ ]+' "$LOG" | awk '{print $3}' | sort -u)
n=$(printf '%s\n' "$paths" | grep -c . || true)
echo "== v2 client made $n distinct request paths =="
if [ "$n" -lt 4 ]; then
  echo "v2cli-check: client made too few requests ($n) — did it boot?" >&2
  for f in "$DATA"/cmd_*.out; do echo "--- $f ---"; tail -10 "$f"; done >&2
  exit 2
fi

html=0
for p in $paths; do
  case "$p" in
    /global/event|/event|/api/event|/api/log|*/connect) continue ;;
  esac
  ct=$(curl -s -o /dev/null -w '%{content_type}' --max-time 5 "http://127.0.0.1:$PORT$p")
  case "$ct" in
    *text/html*) echo "HTML  $p  ($ct)"; html=$((html+1)) ;;
  esac
done

info_ct=$(curl -s -o /dev/null -w '%{content_type}' --max-time 5 "http://127.0.0.1:$PORT/api/info")
echo "== /api/info content-type: $info_ct =="

if [ "$html" -ne 0 ]; then
  echo "FAIL: $html v2-client-reachable route(s) served HTML (the crash class)"
  exit 1
fi
case "$info_ct" in
  *application/json*) ;;
  *) echo "FAIL: /api/info not JSON ($info_ct) — v2 surface dark"; exit 1 ;;
esac
echo "ok: no v2-client-reachable route served HTML; /api/info is JSON"
exit 0
