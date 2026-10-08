#!/usr/bin/env bash
# Wire-corpus replay gate (DIFFERENTIATION.md §5 P1a / D4).
#
# Boots ocserve on an ephemeral port with a throwaway data dir, runs the
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

# Position-independent corpus: the binary's default is compile-time baked
# (CARGO_MANIFEST_DIR), so a moved/renamed checkout breaks replay with a
# bare "read manifest.json". The gate always names its corpus explicitly.
export OCSERVE_CORPUS="${OCSERVE_CORPUS:-$PWD/testdata/golden}"

PORT="${OCSERVE_CHECK_PORT:-4919}"
BIN="${OCSERVE_BIN:-target/debug/ocserve}"
SELF_TEST=0
[ "${1:-}" = "--self-test" ] && SELF_TEST=1

[ -x "$BIN" ] || { echo "replay-check: $BIN missing (cargo build -p ocserve)" >&2; exit 2; }

DATA="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-replay-check.XXXXXX")"
CORPUS_COPY=""
cleanup() {
  # Kill the PROCESS GROUP (spawned via setsid) — ocserve's plugin-host node
  # child dies with it. Plain $PID kill orphaned 34 node hosts / 717MB over
  # repeated runs (census 2026-10-04).
  if [ -n "${PID:-}" ]; then
    kill -- "-$PID" 2>/dev/null || kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  [ -n "${CORPUS_COPY:-}" ] && rm -rf "$CORPUS_COPY"
  rm -rf "$DATA"
}
trap cleanup EXIT
# Hygiene: sweep stale replay plugin-hosts from prior crashed runs (the
# /tmp/ocserve-repl prefix is ocserve-plugin's throwaway host dir — never a
# production path).
pkill -f "/tmp/ocserve-repl" 2>/dev/null || true

# Fixture HOME (2026-10-08, CI finding): the recorded corpus encodes
# config-derived routes (agent/command keys, /config/providers defaults,
# /mcp statuses), so the corpus oracle only holds under a config that
# carries those keys. Booting with the ambient HOME made this gate
# machine-dependent: green on a developer box, red on a bare runner whose
# HOME has no opencode config. The COMMITTED fixture (testdata/fixture,
# recorded by scripts/record-corpus.sh) is the one the corpus was recorded
# under, so the gate means the same thing everywhere and on CI.
FH="$DATA/fixture-home"
cp -r "$PWD/testdata/fixture" "$FH"

setsid env HOME="$FH" OCSERVE_DATA_DIR="$DATA" OCSERVE_LEGACY_SYNC=0 \
  OPENCODE_DISABLE_MODELS_FETCH=1 "$BIN" serve \
  --port "$PORT" >/dev/null 2>&1 &
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

run_replay() { # $1 = optional OCSERVE_CORPUS dir; returns replay exit code
  local out rc=0
  if [ -n "${1:-}" ]; then
    out="$(OCSERVE_CORPUS="$1" "$BIN" replay --target "http://127.0.0.1:$PORT" 2>&1)" || rc=$?
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
  CORPUS_COPY="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-corpus.XXXXXX")"
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
