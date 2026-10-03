#!/usr/bin/env bash
# Upstream drift watch (PLAN §6 "nightly replays against latest upstream",
# PLAN §8 adoption posture). READ-ONLY TRIAGE — never auto-adopts.
#
# Method: dual-arm A/B against the recorded corpus (testdata/golden, 31 GET
# routes, 6 declared deferrals skipped):
#   control arm = freeze image (parity-upstream = opencode 1.18.31), replayed
#                 TWICE — unequal failure sets mean harness noise → abort;
#   latest arm  = npm `latest` release (Dockerfile.upstream-npm), same apt
#                 set + same config mounts + fresh data — so arm-vs-arm
#                 differences are VERSION drift, not environment drift.
#   drift  = fail(latest) \ fail(control)   ← triage: adopt / ignore
#   noise  = fail(latest) ∩ fail(control)   ← environment, not version
#   anomaly= fail(control) \ fail(latest)   ← harness/env bug, reported red
#
# Coverage honesty: GET-corpus only (31 routes). POST/SSE/agent-loop drift is
# owned by golden tests + field reports; the PLAN §6 kill criterion (>20%
# divergences unrecordable → contract-by-probe) is evaluated on this report.
#
# Usage: scripts/drift-watch.sh [--selftest] [--freeze-only]
#   --selftest    offline unit checks of the pure helpers (guard rule 5)
#   --freeze-only control arm only (validate the harness; no npm/docker build)
# Env: FROZEN (default 1.18.31 = PLAN §3 freeze), REFINE_BIN, DRIFT_PORT_F
#      (4923), DRIFT_PORT_L (4924), FREEZE_RUNS (2), DRIFT_REPORT_DIR
# cron: 0 6 * * * cd <repo> && scripts/drift-watch.sh >> /tmp/drift-watch.log 2>&1
# Exit: 0 = report written (drift found is INFORMATION, not failure)
#       1 = harness failure (npm/docker/health/replay crash/nondeterminism)
set -euo pipefail
cd "$(dirname "$0")/.."

FROZEN="${FROZEN:-1.18.31}"
REFINE_BIN="${REFINE_BIN:-}"
PORT_F="${DRIFT_PORT_F:-4923}"
PORT_L="${DRIFT_PORT_L:-4924}"
FREEZE_RUNS="${FREEZE_RUNS:-2}"
REPORT_DIR="${DRIFT_REPORT_DIR:-bench/drift}"
CFG="$PWD/bench/parity/config"
IMAGE_FREEZE="parity-upstream"
NAME_FREEZE="drift-freeze"
NAME_LATEST="drift-latest"

# ─────────────────────────── pure helpers (selftested) ───────────────────────────

# version_gt A B → true iff A > B (semver-ish, numeric dot segments)
version_gt() {
  [ "$1" != "$2" ] || return 1
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | tail -n 1)" = "$1" ]
}

# parse_replay OUT PASSLIST FAILLIST — replay stdout → name lists; prints "pass fail defer"
parse_replay() {
  local out="$1" pl="$2" fl="$3"
  : >"$pl"
  : >"$fl"
  awk '
    /^PASS / { p++; sub(/^PASS /,""); split($0,a,/ /); print a[1] > pl; next }
    /^FAIL / { f++; sub(/^FAIL /,""); split($0,a,/:/); print a[1] > fl; next }
    /^DEFER / { d++ }
    END { printf "%d %d %d\n", p+0, f+0, d+0 }
  ' pl="$pl" fl="$fl" "$out"
}

# set_diff A B → lines of A not in B  (both = newline lists)
set_diff() { comm -23 <(sort -u "$1") <(sort -u "$2"); }
set_intersect() { comm -12 <(sort -u "$1") <(sort -u "$2"); }

# report_write FILE freeze latest v2note drift_file noise_file anomaly_file \
#              freeze_counts latest_counts compared
report_write() {
  local file="$1" freeze="$2" latest="$3" v2note="$4" driftf="$5" noisef="$6" \
        anomf="$7" fcounts="$8" lcounts="$9"
  local drift_n noise_n anom_n
  drift_n=$(wc -l <"$driftf")
  noise_n=$(wc -l <"$noisef")
  anom_n=$(wc -l <"$anomf")
  mkdir -p "$(dirname "$file")"
  {
    echo "# upstream drift watch — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    echo "| | |"
    echo "|---|---|"
    echo "| freeze (PLAN §3) | \`opencode ${freeze}\` (control arm, replayed ${FREEZE_RUNS}×) |"
    echo "| latest (npm) | \`opencode ${latest}\` |"
    echo "| v2 on npm | ${v2note} |"
    echo "| control arm counts (pass fail defer) | ${fcounts} |"
    echo "| latest arm counts (pass fail defer) | ${lcounts} |"
    echo "| drift (latest-only failures) | ${drift_n} |"
    echo "| noise (both arms) | ${noise_n} |"
    echo "| anomaly (control-only — investigate) | ${anom_n} |"
    echo
    echo "## drift — triage: adopt / ignore (never auto-merged)"
    if [ "$drift_n" -eq 0 ]; then
      echo "_none — latest matches the freeze on every compared route._"
    else
      echo "| route | latest detail | control |"
      echo "|---|---|---|"
      while IFS= read -r name; do
        [ -n "$name" ] || continue
        local d
        d=$(grep -m1 "^FAIL ${name}:" "${REPORT_DIR}/.latest.out" 2>/dev/null | sed 's/^FAIL [^:]*: //' || echo "?")
        echo "| \`${name}\` | ${d} | pass |"
      done <"$driftf"
    fi
    echo
    echo "## noise (failed in BOTH arms — environment/config, not version)"
    if [ "$noise_n" -eq 0 ]; then echo "_none._"; else
      while IFS= read -r name; do echo "- \`${name}\`"; done <"$noisef"
    fi
    echo
    echo "## anomaly (control arm failed — harness/environment bug, action: investigate)"
    if [ "$anom_n" -eq 0 ]; then echo "_none._"; else
      while IFS= read -r name; do echo "- \`${name}\`"; done <"$anomf"
    fi
    echo
    echo "## coverage & kill criterion (PLAN §6)"
    echo "- GET-corpus only (\`testdata/golden\`, declared deferrals skipped)."
    echo "- POST/SSE/agent-loop drift: owned by golden tests + field reports."
    echo "- kill criterion: if >20% of observed divergences are unrecordable by"
    echo "  this corpus → shift to contract-by-probe."
    echo "- read-only: this report never modifies refine, the freeze, or config."
  } >"$file"
}

# ─────────────────────────── selftest (offline) ───────────────────────────

run_selftest() {
  local bad=0 tmp
  tmp=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  # version_gt: strict, symmetric-safe, cross-major
  version_gt 1.18.34 1.18.31 || { echo "selftest: version_gt 1.18.34>1.18.31 failed"; bad=1; }
  version_gt 1.18.31 1.18.31 && { echo "selftest: version_gt equal must be false"; bad=1; }
  version_gt 1.18.30 1.18.31 && { echo "selftest: version_gt older must be false"; bad=1; }
  version_gt 2.0.0 1.18.34 || { echo "selftest: version_gt cross-major failed"; bad=1; }

  # parse_replay: canned replay stdout → counts + fail list
  printf '%s\n' \
    'PASS agent' \
    'FAIL config: status 400 != recorded 200' \
    'DEFER mcp (declared M4)' \
    'PASS vcs' \
    'FAIL session_list: key paths differ; missing=[] extra=["$"]' \
    'replay x: 2 passed, 2 failed' >"$tmp/out"
  local c
  c=$(parse_replay "$tmp/out" "$tmp/pass" "$tmp/fail")
  [ "$c" = "2 2 1" ] || { echo "selftest: parse_replay counts got '$c' want '2 2 1'"; bad=1; }
  [ "$(wc -l <"$tmp/fail")" -eq 2 ] || { echo "selftest: fail list size"; bad=1; }
  grep -qx "config" "$tmp/fail" || { echo "selftest: fail list content"; bad=1; }

  # set ops: drift / noise / anomaly classification
  printf 'a\nb\nc\n' >"$tmp/L"
  printf 'b\nc\nd\n' >"$tmp/F"
  [ "$(set_diff "$tmp/L" "$tmp/F")" = "a" ] || { echo "selftest: set_diff"; bad=1; }
  [ "$(set_intersect "$tmp/L" "$tmp/F" | tr -d '\n')" = "bc" ] || { echo "selftest: set_intersect"; bad=1; }

  # report writer: structure present, drift row rendered, zero-drift phrase
  printf 'config\n' >"$tmp/drift"
  : >"$tmp/noise"
  : >"$tmp/anom"
  report_write "$tmp/r.md" 1.18.31 1.18.34 "not on npm" "$tmp/drift" "$tmp/noise" "$tmp/anom" "25 0 6" "24 1 6"
  grep -q "# upstream drift watch" "$tmp/r.md" || { echo "selftest: report header"; bad=1; }
  grep -q '`config`' "$tmp/r.md" || { echo "selftest: report drift row"; bad=1; }
  grep -q "kill criterion" "$tmp/r.md" || { echo "selftest: report coverage"; bad=1; }

  if [ "$bad" -eq 0 ]; then echo "drift-watch selftest ok"; fi
  return "$bad"
}

# ─────────────────────────── container harness ───────────────────────────

cleanup() { docker rm -f "$NAME_FREEZE" "$NAME_LATEST" >/dev/null 2>&1 || true; }

resolve_bin() {
  if [ -n "$REFINE_BIN" ]; then :; \
  elif command -v refine >/dev/null 2>&1; then REFINE_BIN="$(command -v refine)"; \
  elif [ -x target/release/refine ]; then REFINE_BIN="$PWD/target/release/refine"; \
  else echo "harness failure: no refine binary (build target/release/refine or set REFINE_BIN)"; exit 1; fi
}

wait_health() {
  local url="$1" i
  for i in $(seq 1 90); do
    if curl -fsS -m 2 "$url/global/health" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "harness failure: $url never healthy"
  return 1
}

start_container() { # $1 name  $2 image  $3 host-port
  docker rm -f "$1" >/dev/null 2>&1 || true
  docker run -d --name "$1" \
    -p "127.0.0.1:$3:4921" \
    -v "$CFG/opencode.json:/root/.config/opencode/opencode.json:ro" \
    -v "$CFG/auth.json:/root/.local/share/opencode/auth.json:ro" \
    -v "$CFG/model.json:/root/.local/state/opencode/model.json:ro" \
    "$2" >/dev/null
}

replay_once() { # $1 url  $2 outfile → echoes "pass fail defer"; crash if no summary
  local rc=0
  "$REFINE_BIN" replay --target "$1" >"$2" 2>&1 || rc=$?
  if ! grep -q '^replay ' "$2"; then
    echo "harness failure: replay produced no summary (rc=$rc):"
    tail -n 5 "$2"
    exit 1
  fi
  grep '^replay ' "$2" | tail -n 1
}

# ─────────────────────────── main ───────────────────────────

MODE="${1:-}"

if [ "$MODE" = "--selftest" ]; then
  run_selftest
  exit $?
fi

resolve_bin
command -v docker >/dev/null || { echo "harness failure: docker missing"; exit 1; }
command -v npm >/dev/null || { echo "harness failure: npm missing"; exit 1; }

echo "== drift watch: querying npm =="
LATEST="$(npm view opencode-ai version 2>/dev/null)" || { echo "harness failure: npm view failed"; exit 1; }
V2="$(npm view 'opencode-ai@>=2.0.0' version 2>/dev/null || true)"
if [ -n "$V2" ]; then V2NOTE="**PUBLISHED: ${V2}** — PLAN §8 adoption trigger review"; else V2NOTE="not on npm (pre-release tags only)"; fi
echo "latest=${LATEST}  freeze=${FROZEN}  v2=${V2NOTE}"

cleanup
trap cleanup EXIT

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$REPORT_DIR"
WORK="$(mktemp -d)"
trap 'cleanup; rm -rf "$WORK"' EXIT

# control arm: freeze image × FREEZE_RUNS (determinism gate)
if [ ! -d "$CFG" ]; then echo "harness failure: $CFG missing"; exit 1; fi
if ! docker image inspect "$IMAGE_FREEZE" >/dev/null 2>&1; then
  echo "freeze image missing — building via parity compose"
  (cd bench/parity && ROUND=drift docker compose -f compose.yaml build upstream) >&2 || {
    echo "harness failure: cannot build freeze image"; exit 1; }
fi
echo "== control arm: opencode ${FROZEN} (replay ×${FREEZE_RUNS}) =="
start_container "$NAME_FREEZE" "$IMAGE_FREEZE" "$PORT_F"
wait_health "http://127.0.0.1:$PORT_F"
# first-boot warmup: async catalog/agent loads settle on the first replay —
# discard it so measured runs compare stable state (determinism gate below
# caught exactly this: api_agent/api_command failed run 1 only, not run 2)
replay_once "http://127.0.0.1:$PORT_F" "$WORK/warmup.freeze.out" >/dev/null
echo "warmup replay (discarded) ok"
FCOUNTS=""
declare -a FSETS=()
for i in $(seq 1 "$FREEZE_RUNS"); do
  replay_once "http://127.0.0.1:$PORT_F" "$WORK/freeze.$i.out" >/dev/null
  c="$(parse_replay "$WORK/freeze.$i.out" "$WORK/freeze.$i.pass" "$WORK/freeze.$i.fail")"
  FSETS+=("$(sort -u "$WORK/freeze.$i.fail")")
  [ -n "$FCOUNTS" ] || FCOUNTS="$c"
done
# determinism: every control replay must fail the same route set
for i in $(seq 2 "$FREEZE_RUNS"); do
  if [ "${FSETS[0]}" != "${FSETS[$((i-1))]}" ]; then
    echo "harness failure: control arm nondeterministic (run 1 vs run $i) — cannot claim drift"
    diff <(echo "${FSETS[0]}") <(echo "${FSETS[$((i-1))]}") || true
    exit 1
  fi
done
echo "control deterministic ✓ (${FREEZE_RUNS}× identical failure set)"
docker rm -f "$NAME_FREEZE" >/dev/null 2>&1 || true
cp "$WORK/freeze.1.out" "$REPORT_DIR/.freeze.out"
: >"$WORK/freeze.fail"
cat "$WORK/freeze.1.fail" | sort -u >"$WORK/freeze.fail"

if [ "$MODE" = "--freeze-only" ]; then
  echo "freeze-only: control arm pass. (no latest probe)"
  exit 0
fi

if [ "$LATEST" = "$FROZEN" ]; then
  echo "== latest == freeze (${LATEST}) — nothing to watch =="
  : >"$WORK/drift"; : >"$WORK/noise"; : >"$WORK/anom"
  report_write "$REPORT_DIR/${STAMP}-upstream-drift.md" "$FROZEN" "$LATEST" "$V2NOTE" \
    "$WORK/drift" "$WORK/noise" "$WORK/anom" "$FCOUNTS" "same-version"
  echo "report: $REPORT_DIR/${STAMP}-upstream-drift.md"
  exit 0
fi

# latest arm
echo "== latest arm: opencode ${LATEST} (npm) =="
docker build -q -f bench/parity/Dockerfile.upstream-npm \
  --build-arg OCV="$LATEST" -t "drift-upstream:${LATEST}" bench/parity >/dev/null || {
  echo "harness failure: latest image build failed"; exit 1; }
start_container "$NAME_LATEST" "drift-upstream:${LATEST}" "$PORT_L"
wait_health "http://127.0.0.1:$PORT_L"
# same warmup protocol as the control arm (arm symmetry)
replay_once "http://127.0.0.1:$PORT_L" "$WORK/warmup.latest.out" >/dev/null
echo "warmup replay (discarded) ok"
replay_once "http://127.0.0.1:$PORT_L" "$REPORT_DIR/.latest.out"
LCOUNTS="$(parse_replay "$REPORT_DIR/.latest.out" "$WORK/latest.pass" "$WORK/latest.fail")"
docker rm -f "$NAME_LATEST" >/dev/null 2>&1 || true

sort -u "$WORK/latest.fail" >"$WORK/latest.fail.sorted"
set_diff "$WORK/latest.fail.sorted" "$WORK/freeze.fail" >"$WORK/drift"
set_intersect "$WORK/latest.fail.sorted" "$WORK/freeze.fail" >"$WORK/noise"
set_diff "$WORK/freeze.fail" "$WORK/latest.fail.sorted" >"$WORK/anom"

REPORT="$REPORT_DIR/${STAMP}-upstream-drift.md"
report_write "$REPORT" "$FROZEN" "$LATEST" "$V2NOTE" \
  "$WORK/drift" "$WORK/noise" "$WORK/anom" "$FCOUNTS" "$LCOUNTS"

echo "== summary =="
echo "drift: $(wc -l <"$WORK/drift")  noise: $(wc -l <"$WORK/noise")  anomaly: $(wc -l <"$WORK/anom")"
[ "$(wc -l <"$WORK/anom")" -eq 0 ] || { echo "FAIL: control-only failures (anomaly) — investigate before trusting drift"; cat "$WORK/anom"; exit 1; }
echo "report: $REPORT"
