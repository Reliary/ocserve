#!/usr/bin/env bash
# Permission differential (behavioural compatibility; guard rule 18).
#
# Route-binding (rule 16) and response-shape (P4) guards structurally cannot
# see authorization SEMANTICS. This boots freeze and ocserve under the SAME
# fixture HOME (no config permission block → both use native defaults) and
# replays the committed permission oracle vectors:
#
#   POST /api/session/{id}/permission  → {id, effect}
#
# is a PURE permission oracle (no LLM, no tool execution), so the vectors are
# a cheap, deterministic differential target (McKeeman differential testing).
# The vectors are generated from freeze by `gen-vectors.py` and committed; a
# mismatch means ocserve's evaluate/ruleset-derivation drifted.
#
# Note on scope: the oracle covers `evaluate(action, resource, ruleset)` and
# ruleset derivation. The TOOL-LEVEL ask sequence (arity prefixes, external
# directories) is not reachable without a model turn; it is covered by the
# pinned-arity property tests in `ocserve-tools` (bash_patterns) and the
# `permission_asks` behaviour tests. A live tool-level differential needs the
# stub-model turn harness (parked, PLAN §15).
#
# Usage: scripts/permission-check.sh [--self-test]
# Exit 0 = ocserve matches freeze on every vector; 1 = divergence.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT_F="${PERM_PORT_F:-4928}"
PORT_R="${PERM_PORT_R:-4929}"
VENDOR="bench/parity/vendor/opencode"
OCSERVE_BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
VECTORS="bench/permission/1.18.31.vectors.json"
SELF_TEST="${1:-}"

die() { echo "permission-check: $*" >&2; exit 2; }

[ -x "$VENDOR" ] || die "$VENDOR missing (pair-check stages it; run that once)"
[ -x "$OCSERVE_BIN" ] || die "$OCSERVE_BIN missing (cargo build --release)"
[ -f "$VECTORS" ] || die "$VECTORS missing (run gen-vectors.py)"

# port preflight
python3 - "$PORT_F" "$PORT_R" <<'PY' || die "ports busy ($PORT_F/$PORT_R)"
import socket, sys
for p in map(int, sys.argv[1:]):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try: s.bind(("127.0.0.1", p))
    except OSError: sys.exit(1)
    finally: s.close()
PY

HF=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-perm-freeze.XXXXXX")
HR=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-perm-ocserve.XXXXXX")
# Both arms run the SAME flat-action permission fixture: the V2 oracle honors
# flat action rules (edit/read/bash/…) but, unlike V1, ignores sub-pattern
# rules — so the differential is scoped to the shared model (documented
# divergence D-PERM-V2-SUBPATTERN, PLAN §17).
PERM_CFG="$PWD/bench/permission/config/opencode.json"
[ -f "$PERM_CFG" ] || die "fixture config missing: $PERM_CFG"
for h in "$HF" "$HR"; do
  mkdir -p "$h/.config/opencode" "$h/.local/share/opencode" "$h/.local/state/opencode" "$h/.cache/opencode"
  cp "$PERM_CFG" "$h/.config/opencode/opencode.json"
done

setsid env HOME="$HF" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$VENDOR" serve --port "$PORT_F" --hostname 127.0.0.1 >"$HF/freeze.log" 2>&1 &
PID_F=$!
setsid env HOME="$HR" OPENCODE_DISABLE_MODELS_FETCH=1 OCSERVE_DATA_DIR="$HR/.local/share/ocserve" \
  OCSERVE_LEGACY_SYNC=0 "$OCSERVE_BIN" serve --port "$PORT_R" >"$HR/ocserve.log" 2>&1 &
PID_R=$!
cleanup() {
  kill -- -"$PID_F" 2>/dev/null; kill -- -"$PID_R" 2>/dev/null
  pkill -f "opencode serve --port $PORT_F" 2>/dev/null
  pkill -f "ocserve serve --port $PORT_R" 2>/dev/null
  rm -rf "$HF" "$HR"
}
trap cleanup EXIT

wait_health() { # $1 port $2 label
  for _ in $(seq 1 90); do
    curl -s --max-time 2 "http://127.0.0.1:$1/global/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "permission-check: $2 never healthy" >&2; return 1
}
wait_health "$PORT_F" freeze || { cat "$HF/freeze.log"; exit 2; }
wait_health "$PORT_R" ocserve || { cat "$HR/ocserve.log"; exit 2; }

if [ "$SELF_TEST" = "1" ]; then
  echo "== self-test: clean replay must PASS =="
  if python3 bench/permission/permission-replay.py "$VECTORS" "http://127.0.0.1:$PORT_R"; then
    echo "self-test OK (clean green)"
    exit 0
  fi
  echo "self-test: clean replay FAILED (gate red before sabotage)" >&2
  exit 3
fi

python3 bench/permission/permission-replay.py "$VECTORS" "http://127.0.0.1:$PORT_R"
rc=$?
if curl -s --max-time 2 http://127.0.0.1:4901/global/health >/dev/null 2>&1; then
  echo "live opencode :4901 still healthy"
fi
if curl -s --max-time 2 http://127.0.0.1:4912/global/health >/dev/null 2>&1; then
  echo "live ocserve :4912 still healthy"
fi
exit "$rc"
