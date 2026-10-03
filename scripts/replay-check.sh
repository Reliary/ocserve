#!/usr/bin/env bash
# Wire-corpus replay gate (DIFFERENTIATION.md §5 P1a / D4).
#
# Boots refine on an ephemeral port with a throwaway data dir, runs the
# differential wire-corpus replay against it, and exits non-zero on any FAIL.
# Use it after config/route-affecting changes: `./scripts/replay-check.sh`.
#
# Exit codes: 0 = replay green; 1 = replay failures; 2 = infrastructure
# (binary missing, health timeout); 3 = self-test negative control failed.
#
# --self-test: proves BOTH exits — clean corpus must pass, a sabotaged body
# must fail (planting the negative control, TESTING §1 anti-theater).
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${REFINE_CHECK_PORT:-4919}"
BIN="${REFINE_BIN:-target/debug/refine}"
SELF_TEST=0
[ "${1:-}" = "--self-test" ] && SELF_TEST=1

[ -x "$BIN" ] || { echo "replay-check: $BIN missing (cargo build -p refine-cli)" >&2; exit 2; }

DATA="$(mktemp -d "${TMPDIR:-/tmp}/refine-replay-check.XXXXXX")"
CORPUS_COPY=""
cleanup() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null || true
  [ -n "${CORPUS_COPY:-}" ] && rm -rf "$CORPUS_COPY"
  rm -rf "$DATA"
}
trap cleanup EXIT

REFINE_DATA_DIR="$DATA" REFINE_LEGACY_SYNC=0 "$BIN" serve --port "$PORT" \
  >/dev/null 2>&1 &
PID=$!

ready() {
  python3 - "$PORT" <<'PY'
import sys, urllib.request
try:
    urllib.request.urlopen(f"http://127.0.0.1:{sys.argv[1]}/global/health", timeout=1).read()
except Exception:
    sys.exit(1)
PY
}
for _ in $(seq 1 80); do
  if ready; then break; fi
  kill -0 "$PID" 2>/dev/null || { echo "replay-check: serve died during boot" >&2; exit 2; }
  sleep 0.25
done
ready || { echo "replay-check: health timeout on :$PORT" >&2; exit 2; }

# Seed one session: the recorded corpus was captured with sessions present;
# an empty DB answers [] and the key-path projection legitimately differs.
seed_session() {
  python3 - "$PORT" <<'PY'
import sys, urllib.request, json
req = urllib.request.Request(
    f"http://127.0.0.1:{sys.argv[1]}/session",
    data=json.dumps({"title": "OpenCode replay seed"}).encode(),
    headers={"Content-Type": "application/json"},
    method="POST",
)
urllib.request.urlopen(req, timeout=5).read()
PY
}

run_replay() { # $1 = optional REFINE_CORPUS dir; returns replay exit code
  local out rc=0
  if [ -n "${1:-}" ]; then
    out="$(REFINE_CORPUS="$1" "$BIN" replay --target "http://127.0.0.1:$PORT" 2>&1)" || rc=$?
  else
    out="$("$BIN" replay --target "http://127.0.0.1:$PORT" 2>&1)" || rc=$?
  fi
  printf '%s\n' "$out"
  return "$rc"
}

seed_session || { echo "replay-check: seed failed" >&2; exit 2; }

if [ "$SELF_TEST" = "1" ]; then
  echo "== self-test: clean corpus must PASS =="
  rc=0; run_replay "" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "replay-check self-test: clean corpus FAILED (rc=$rc) — gate is red before sabotage" >&2
    exit 3
  fi
  echo "== self-test: sabotaged corpus must FAIL =="
  CORPUS_COPY="$(mktemp -d "${TMPDIR:-/tmp}/refine-corpus.XXXXXX")"
  cp -r testdata/golden/. "$CORPUS_COPY/"
  # corrupt a byte-mode body (global_health is recorded byte-exact)
  printf 'CORRUPTED' >> "$CORPUS_COPY/global_health.body"
  rc=0; run_replay "$CORPUS_COPY" >/dev/null || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "replay-check self-test: sabotaged corpus PASSED — negative control dead" >&2
    exit 3
  fi
  echo "self-test OK: clean=0 sabotaged=$rc"
  exit 0
fi

run_replay ""
