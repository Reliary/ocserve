#!/usr/bin/env bash
# Live TUI attach acceptance (2026-10-09).
#
# The 2026-10-08/09 bug class: ocserve's catch-all served the SPA HTML (or the
# app.opencode.ai proxy) for routes the TUI actually calls, and a status-only
# "no 404s" check counted a 200 HTML body as success. This runs the REAL
# `opencode attach` (freeze 1.18.31) against a fixture-booted ocserve, captures
# every request path the TUI makes from the server's hit-set log, then replays
# each path and asserts it is served JSON — not text/html.
#
# Exit 0 when no TUI-reachable route is served HTML. Any HTML route is the
# crash class and fails the gate. Manual/nightly (not per-commit: it drives a
# full TUI).
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${OCSERVE_TUI_PORT:-5117}"
DATA="$(mktemp -d /tmp/ocserve-tui.XXXXXX)"
LOG="$DATA/serve.log"
OCSERVE_BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
OPENCODE_BIN="${OPENCODE_BIN:-/home/linuxbrew/.linuxbrew/bin/opencode}"
ATTACH_SECS="${OCSERVE_TUI_SECS:-12}"

cleanup() {
  [ -n "${PID:-}" ] && kill -- -"$PID" 2>/dev/null
  pkill -f "ocserve serve --port $PORT" 2>/dev/null
  rm -rf "$DATA"
}
trap cleanup EXIT

if [ ! -x "$OCSERVE_BIN" ]; then
  echo "tui-check: build release first ($OCSERVE_BIN missing)" >&2
  exit 2
fi

FH="$DATA/fixture-home"
cp -r "$PWD/testdata/fixture" "$FH"

setsid env HOME="$FH" OCSERVE_DATA_DIR="$DATA" OCSERVE_LEGACY_SYNC=0 \
  OPENCODE_DISABLE_MODELS_FETCH=1 RUST_LOG=info "$OCSERVE_BIN" serve \
  --port "$PORT" >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 80); do
  curl -s --max-time 1 "http://127.0.0.1:$PORT/global/health" >/dev/null && break
  kill -0 "$PID" 2>/dev/null || { echo "tui-check: serve died"; cat "$LOG"; exit 2; }
  sleep 0.25
done

# Seed a session so the TUI has something to hydrate (the sidebar/diff path).
curl -s -X POST -H 'content-type: application/json' \
  -d '{"title":"tui-check seed"}' "http://127.0.0.1:$PORT/session" >/dev/null

# Run the real TUI against ocserve. It is interactive; we drive it for a fixed
# window and let it exit via timeout (SIGTERM → TUI restores the terminal).
echo "== running opencode attach for ${ATTACH_SECS}s =="
timeout --signal=TERM "$ATTACH_SECS" "$OPENCODE_BIN" attach "http://127.0.0.1:$PORT" \
  --pure --print-logs --log-level ERROR </dev/null >"$DATA/attach.out" 2>&1 || true

# Extract every request path the TUI made (the server logs `req METHOD PATH`).
paths=$(grep -oE 'req [A-Z]+ [^ ]+' "$LOG" | awk '{print $3}' | sort -u)
n=$(printf '%s\n' "$paths" | grep -c . || true)
echo "== TUI made $n distinct request paths =="

if [ "$n" -lt 5 ]; then
  echo "tui-check: TUI made too few requests ($n) — attach likely failed to boot" >&2
  echo "--- attach output (tail) ---"; tail -20 "$DATA/attach.out" >&2
  echo "--- serve log (tail) ---"; tail -20 "$LOG" >&2
  exit 2
fi

# Replay each TUI path and assert JSON (not HTML). Skip paths that are not
# GET-able in isolation (SSE streams) and the health probe.
html=0
for p in $paths; do
  case "$p" in
    /global/event|/event|/api/event|*/connect) continue ;;
  esac
  ct=$(curl -s -o /dev/null -w '%{content_type}' --max-time 5 "http://127.0.0.1:$PORT$p")
  case "$ct" in
    *text/html*) echo "HTML  $p  ($ct)"; html=$((html+1)) ;;
  esac
done

# The specific proven crash path must return a JSON array.
diff_ct=$(curl -s -o /dev/null -w '%{content_type}' --max-time 5 \
  "http://127.0.0.1:$PORT/session/$(curl -s "http://127.0.0.1:$PORT/session" | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')/diff")
echo "== /session/{id}/diff content-type: $diff_ct =="

if [ "$html" -ne 0 ]; then
  echo "FAIL: $html TUI-reachable route(s) served HTML (the crash class)"
  exit 1
fi
case "$diff_ct" in
  *application/json*) ;;
  *) echo "FAIL: session diff not JSON ($diff_ct)"; exit 1 ;;
esac
echo "ok: no TUI-reachable route served HTML"
exit 0
