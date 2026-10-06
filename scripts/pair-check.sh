#!/usr/bin/env bash
# pair-check — LIVE freeze ↔ refine direct differential (manual tool).
#
# What it is: dual-arm integration test against the upstream freeze. Vendored
# opencode 1.18.31 and $REFINE_BIN boot with IDENTICAL fixture environments
# (bench/parity/config copied into per-arm temp HOMEs), same cwd (repo root),
# same seed — then `refine replay --pair` compares every manifest route
# DIRECTLY between the two live servers. That direct diff is the GATE.
# Recorded-corpus freshness prints as info only: the recorded corpus came
# from the real user environment, so fixture-env noise is expected (drift
# control arm measured 19 pass / 7 fail / 5 defer — never gate on it).
#
# Isolation (hard rules): temp HOMEs only — never the real ~/.config or
# legacy state; ports 4926/4927 (live services untouched); process-group
# cleanup; python-only HTTP (no curl).
#
# Exits: 0 = pair green | 1 = divergence | 2 = infra | 3 = self-test failed
#
#   ./scripts/pair-check.sh              # gate run
#   ./scripts/pair-check.sh --self-test  # proves comparator not vacuous:
#                                         # clean run must pass, an extra
#                                         # session planted on ONE arm must
#                                         # make the gate fail (red→green)
#
# Allowlist (only with a named divergence row): REFINE_PAIR_ALLOW=file of
# `name # D-ROW reason` lines — read by the replay engine, never silently.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT_F=4926
PORT_R=4927
REFINE_BIN="${REFINE_BIN:-target/release/refine}"
SELF_TEST=0
[ "${1:-}" = "--self-test" ] && SELF_TEST=1

HF=""; HR=""; PID_F=""; PID_R=""
cleanup() {
  for p in "$PID_R" "$PID_F"; do
    [ -n "$p" ] && { kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null || true; }
  done
  [ -n "$HF" ] && rm -rf "$HF"
  [ -n "$HR" ] && rm -rf "$HR"
}
trap cleanup EXIT
# stale pair hosts from prior crashed runs (same class as replay-check)
pkill -f "/tmp/refine-pair" 2>/dev/null || true

say() { printf '%s\n' "$*"; }
die2() { echo "pair-check: $*" >&2; exit 2; }

# ---------- preflight ----------
[ -x "$REFINE_BIN" ] || die2 "$REFINE_BIN missing (cargo build --release)"
VENDOR="bench/parity/vendor/opencode"
if [ ! -x "$VENDOR" ]; then
  src=$(command -v opencode 2>/dev/null || true)
  [ -n "$src" ] || src=$(readlink -f /home/linuxbrew/.linuxbrew/bin/opencode 2>/dev/null || true)
  [ -n "$src" ] && [ -x "$src" ] || die2 "vendor/opencode missing and no host opencode found"
  say "staging freeze binary from $src"
  cp "$src" "$VENDOR" && chmod +x "$VENDOR"
fi
ver=$("$VENDOR" --version 2>/dev/null | head -1)
[ "$ver" = "1.18.31" ] || die2 "freeze binary reports '$ver', want 1.18.31"

python3 - "$PORT_F" "$PORT_R" <<'PY' || die2 "ports busy (4926/4927 required)"
import socket, sys
for p in map(int, sys.argv[1:]):
    s = socket.socket()
    # TIME_WAIT from a run seconds ago must not trip the preflight
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", p))
    except OSError:
        sys.exit(1)
    finally:
        s.close()
PY

# live services must be healthy before AND after (isolation evidence)
live_ok() {
  python3 - "$1" <<'PY'
import sys, urllib.request
try:
    urllib.request.urlopen(f"http://127.0.0.1:{sys.argv[1]}/global/health", timeout=2)
except Exception:
    sys.exit(1)
PY
}

CFG="$PWD/bench/parity/config"
[ -f "$CFG/opencode.json" ] || die2 "fixture config missing: $CFG"
HF=$(mktemp -d "${TMPDIR:-/tmp}/refine-pair-freeze.XXXXXX")
HR=$(mktemp -d "${TMPDIR:-/tmp}/refine-pair-refine.XXXXXX")
for h in "$HF" "$HR"; do
  mkdir -p "$h/.config/opencode" "$h/.local/share/opencode" "$h/.local/state/opencode" "$h/.cache/opencode"
  cp "$CFG/opencode.json" "$h/.config/opencode/opencode.json"
  cp "$CFG/auth.json" "$h/.local/share/opencode/auth.json"
  cp "$CFG/model.json" "$h/.local/state/opencode/model.json"
  # Symmetric catalog: freeze serves ~215 providers from its INTERNAL
  # registry even with fetch off (proven: no models.json written), while
  # refine's registry is catalog-backed — so BOTH arms get the same real
  # models cache copied in (fetch stays disabled on both = hermetic).
  REAL_CACHE="$HOME/.cache/opencode/models.json"
  if [ -f "$REAL_CACHE" ]; then
    cp "$REAL_CACHE" "$h/.cache/opencode/models.json"
    touch "$h/.cache/opencode/models.json"
  fi
done

say "== spawn (identical fixture HOMEs, cwd=repo, models fetch off) =="
setsid env HOME="$HF" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$VENDOR" serve --port "$PORT_F" --hostname 127.0.0.1 >"$HF/freeze.log" 2>&1 &
PID_F=$!
setsid env HOME="$HR" OPENCODE_DISABLE_MODELS_FETCH=1 \
  REFINE_DATA_DIR="$HR/.local/share/refine" REFINE_LEGACY_SYNC=0 \
  "$REFINE_BIN" serve --port "$PORT_R" >"$HR/refine.log" 2>&1 &
PID_R=$!

wait_health() { # $1 port $2 label
  python3 - "$1" "$2" <<'PY' || { echo "pair-check: $2 never healthy" >&2; exit 2; }
import sys, time, urllib.request
port, label = sys.argv[1], sys.argv[2]
for _ in range(90):
    try:
        r = urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health", timeout=2)
        import json
        v = json.loads(r.read()).get("version")
        if v != "1.18.31":
            print(f"pair-check: {label} reports version {v}, want 1.18.31", file=sys.stderr)
            sys.exit(2)
        sys.exit(0)
    except SystemExit:
        raise
    except Exception:
        time.sleep(1)
sys.exit(2)
PY
}
wait_health "$PORT_F" freeze || { tail -20 "$HF/freeze.log" >&2; exit 2; }
wait_health "$PORT_R" refine || { tail -20 "$HR/refine.log" >&2; exit 2; }
say "both arms healthy on 1.18.31"

seed() { # $1 port — identical seed on both (recorded corpus assumed sessions)
  python3 - "$1" <<'PY'
import json, sys, urllib.request
req = urllib.request.Request(
    f"http://127.0.0.1:{sys.argv[1]}/session",
    data=json.dumps({"title": "pair seed"}).encode(),
    headers={"Content-Type": "application/json"},
    method="POST",
)
urllib.request.urlopen(req, timeout=5).read()
PY
}
seed "$PORT_F" || die2 "seed freeze failed"
seed "$PORT_R" || die2 "seed refine failed"

run_pair() { # → prints engine output; returns its rc
  local rc=0
  # Divergences pass ONLY via the allowlist file (name # D-PAIR-… reason) —
  # read by the engine; every entry must cite a named pair-finding row.
  if [ -f bench/pair/allow.txt ]; then
    REFINE_PAIR_ALLOW="$PWD/bench/pair/allow.txt" \
      "$REFINE_BIN" replay --target "http://127.0.0.1:$PORT_F" \
        --pair "http://127.0.0.1:$PORT_R" 2>&1 || rc=$?
  else
    "$REFINE_BIN" replay --target "http://127.0.0.1:$PORT_F" \
      --pair "http://127.0.0.1:$PORT_R" 2>&1 || rc=$?
  fi
  return "$rc"
}

if [ "$SELF_TEST" = 1 ]; then
  say "== self-test: clean pair (with allowlist) must PASS =="
  rc=0; out="$(run_pair)" || rc=$?
  printf '%s\n' "$out" | tail -5
  if [ "$rc" -ne 0 ]; then
    say "self-test: clean pair FAILED (rc=$rc) — gate red before sabotage"
    exit 3
  fi
  say "== self-test: diverged config on refine must FAIL the gate =="
  # Sabotage the gated route itself (config is NOT allowlisted): add a key
  # to refine's fixture config AFTER boot — H1's 2s poller hot-reloads the
  # payload, so /config keys diverge while freeze stays untouched.
  python3 - "$HR/.config/opencode/opencode.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["pairselftest"] = True
json.dump(c, open(p, "w"), indent=2)
PY
  sleep 5  # REFINE_CONFIG_POLL_MS=2000 default ×2 + margin
  rc=0; out="$(run_pair)" || rc=$?
  printf '%s\n' "$out" | grep -E 'PAIR-DIVERGE|compared' | tail -4 || true
  if [ "$rc" -eq 0 ]; then
    say "self-test: sabotage NOT detected — comparator is vacuous"
    exit 3
  fi
  printf '%s\n' "$out" | grep -q 'PAIR-DIVERGE config' || {
    say "self-test: gate went red but NOT on the sabotaged route (want PAIR-DIVERGE config)"
    exit 3
  }
  say "self-test OK (clean green + config sabotage detected: rc=$rc)"
  exit 0
fi

rc=0; run_pair || rc=$?
if live_ok 4901; then say "live opencode :4901 still healthy"; else say "WARN: live opencode :4901 not reachable (was it up before?)"; fi
if live_ok 4912; then say "live refine :4912 still healthy"; else say "WARN: live refine :4912 not reachable"; fi
exit "$rc"
