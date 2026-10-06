#!/usr/bin/env bash
# load-test — k6 concurrency/load suite: refine vs upstream freeze (MANUAL).
#
# What it measures (L1): server-side resource efficiency, throughput and
# concurrency breakpoints of BOTH servers under identical READ load — zero
# LLM-provider traffic by design (no prompts in the measured window; the
# deterministic stub belongs to L2 only, see L2-DESIGN.md).
#   - read-hot ramp: concurrency ladder (concurrent CONNECTIONS), each VU
#     pinned to its own seeded session (concurrent SESSIONS capacity) plus
#     a hot pass against the 32k-message deep session (contention)
#   - optional arrival: fixed offered rate → achieved-vs-offered (queueing)
#
# Isolation (hard):
#   - k6 targets ONLY 127.0.0.1:4930/4931 (fixture arms) — live :4912/:4901
#     are health-asserted before/after and statically banned from k6 lines
#     (check-guards rule 13)
#   - arms boot in fixture homes; REFINE_LEGACY_SYNC=0; legacy db is
#     read-only snapshot source (mode=ro)
#   - quiet-host gate (MAX_LOAD1, 3 consecutive) + parity-collision refusal
#   - fixtures contain REAL session data: ephemeral, gitignored, removed
#     only by --clean (they exist to be reused between runs)
#
# Usage:
#   scripts/load-test.sh                  # baseline (informational: --no-thresholds)
#   GATED=1 scripts/load-test.sh          # enforce bench/load/thresholds.json
#   ROUNDS=2 LOAD_SESSIONS=200 LOAD_TARGETS=10,25 LOAD_ARRIVAL=1 \
#     LOAD_RPS=200 scripts/load-test.sh
#   scripts/load-test.sh --self-test      # threshold/exit-code wiring (no server)
#   scripts/load-test.sh --clean          # remove fixtures (real data)
#
# Exit: 0 green/baseline | 1 gated-threshold breach | 2 infra | 3 selftest
set -euo pipefail
cd "$(dirname "$0")/.."

PORT_R=4930 # refine arm
PORT_F=4931 # freeze (upstream) arm
K6_IMG="${K6_IMG:-grafana/k6}"
REFINE_BIN="${REFINE_BIN:-target/release/refine}"
VENDOR="bench/parity/vendor/opencode"
CFG="bench/parity/config"
FIX="$PWD/bench/load/.fixtures"
LEGACY_DB="${LEGACY_DB:-$HOME/.local/share/opencode/opencode.db}"
REPO="$PWD"
MAX_LOAD1="${MAX_LOAD1:-2.5}"
ROUNDS="${ROUNDS:-1}"
GATED="${GATED:-0}"
LOAD_SESSIONS="${LOAD_SESSIONS:-200}"
LOAD_TARGETS="${LOAD_TARGETS:-10,25}"
LOAD_ARRIVAL="${LOAD_ARRIVAL:-0}"
LOAD_RPS="${LOAD_RPS:-200}"

SELF_TEST=0 CLEAN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) SELF_TEST=1 ;;
    --clean) CLEAN=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "load-test: unknown flag $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }
die2() { echo "load-test: $*" >&2; exit 2; }

if [ "$CLEAN" = 1 ]; then
  rm -rf "$FIX"
  say "fixtures removed (real-data snapshot + arm homes gone)"
  exit 0
fi

# stale arms from prior crashed runs (never touch live services: their
# cmdlines use --port 4901/4912 and do not match this prefix pattern)
pkill -f 'serve --port 493' 2>/dev/null || true

# ---------- self-test: threshold/exit-code wiring (no server, no gate) -------
if [ "$SELF_TEST" = 1 ]; then
  say "== self-test: k6 image =="
  docker image inspect "$K6_IMG" >/dev/null 2>&1 || die2 "$K6_IMG missing"
  say "== clean thresholds must PASS (rc=0) =="
  docker run --rm -v "$PWD/bench/load:/k6:ro" "$K6_IMG" \
    run /k6/selftest.js >/dev/null 2>&1 || die2 "clean selftest rc!=0 (exit 3 wiring broken)"
  say "== breaching thresholds must FAIL (rc!=0) =="
  if docker run --rm -e LOAD_THRESH_FAIL=1 -v "$PWD/bench/load:/k6:ro" "$K6_IMG" \
    run /k6/selftest.js >/dev/null 2>&1; then
    echo "self-test: breach NOT detected — thresholds cannot gate (exit 3)" >&2
    exit 3
  fi
  say "== --no-thresholds must neutralize the breach (rc=0) =="
  docker run --rm -e LOAD_THRESH_FAIL=1 -v "$PWD/bench/load:/k6:ro" "$K6_IMG" \
    run --no-thresholds /k6/selftest.js >/dev/null 2>&1 \
    || die2 "--no-thresholds did not neutralize breach (exit 3)"
  say "self-test OK (pass / breach / neutralize all proven)"
  exit 0
fi

# ---------- preflight ----------
say "waiting for quiet host (load1 <= $MAX_LOAD1, 3 consecutive samples)…"
quiet=0
while [ "$quiet" -lt 3 ]; do
  l=$(awk '{print $1}' /proc/loadavg)
  awk -v l="$l" -v m="$MAX_LOAD1" 'BEGIN{exit !(l<=m)}' && quiet=$((quiet+1)) || quiet=0
  [ "$quiet" -ge 3 ] || sleep 10
done
say "quiet host ok (load1=$l, gate $MAX_LOAD1)"

# parity collision (both benches gate only at start — never overlap)
if pgrep -f 'lib/runner\.py' >/dev/null 2>&1; then die2 "parity runner active — refuse (sequencing rule)"; fi
if docker ps --format '{{.Names}}' 2>/dev/null | grep -qE 'parity|stub|upstream'; then
  die2 "parity docker stack active — refuse (sequencing rule)"
fi

docker image inspect "$K6_IMG" >/dev/null 2>&1 || die2 "$K6_IMG image missing"
[ -x "$REFINE_BIN" ] || die2 "$REFINE_BIN missing (cargo build --release)"
[ -x "$VENDOR" ] || die2 "$VENDOR missing (stage it: see pair-check)"
[ -f "$CFG/opencode.json" ] || die2 "fixture config missing: $CFG"
[ -r "$LEGACY_DB" ] || die2 "legacy db not readable: $LEGACY_DB (set LEGACY_DB=)"

python3 - "$PORT_R" "$PORT_F" <<'PY' || die2 "ports busy (4930/4931)"
import socket, sys
for p in map(int, sys.argv[1:]):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", p))
    except OSError:
        sys.exit(1)
    finally:
        s.close()
PY

live_ok() {
  python3 - "$1" <<'PY'
import sys, urllib.request
try:
    urllib.request.urlopen(f"http://127.0.0.1:{sys.argv[1]}/global/health", timeout=2)
except Exception:
    sys.exit(1)
PY
}
live_ok 4901 || say "WARN: live opencode :4901 not reachable before run"
live_ok 4912 || say "WARN: live refine :4912 not reachable before run"

# ---------- fixture (real-data snapshot, disk-only) ----------
mkdir -p "$FIX"
need_build=0
if [ ! -f "$FIX/snapshot.db" ] || [ ! -f "$FIX/meta.json" ]; then need_build=1; fi
if [ -f "$FIX/meta.json" ]; then
  have=$(python3 -c "import json;print(json.load(open('$FIX/meta.json'))['sessions'])")
  # meta.sessions = LOAD_SESSIONS + possibly deep (already inside); rebuild when knob changes materially
  [ "$have" -gt $((LOAD_SESSIONS - 1)) ] && [ "$have" -le $((LOAD_SESSIONS + 1)) ] || need_build=1
fi
[ "${FORCE_FIXTURE:-0}" = 1 ] && need_build=1

if [ "$need_build" = 1 ]; then
  say "== fixture: subset snapshot ($LOAD_SESSIONS + deep) from legacy (read-only) =="
  python3 bench/load/make_fixture.py --source "$LEGACY_DB" \
    --dest "$FIX/snapshot.db" --sessions "$LOAD_SESSIONS" \
    --meta "$FIX/meta.json" --cwd "$PWD"
  rm -rf "$FIX/home-freeze" "$FIX/home-refine" # knob change → derived homes stale
fi

META_CWD=$(python3 -c "import json;m=json.load(open('$FIX/meta.json'));print(m['cwd'])")
DEEP=$(python3 -c "import json;print(json.load(open('$FIX/meta.json'))['deep_sid'] or '')")
POOL=$(python3 -c "import json;print(json.load(open('$FIX/meta.json'))['pool_csv'])")

# freeze home: config fresh each run, native db persists (created from snapshot once)
HF="$FIX/home-freeze"; HR="$FIX/home-refine"
mkdir -p "$HF/.config/opencode" "$HF/.local/share/opencode" "$HF/.local/state/opencode" "$HF/.cache/opencode"
mkdir -p "$HR/.config/opencode" "$HR/.local/share/opencode" "$HR/.local/state/opencode" "$HR/.cache/opencode"
cp "$CFG/opencode.json" "$HF/.config/opencode/opencode.json"
cp "$CFG/auth.json" "$HF/.local/share/opencode/auth.json"
cp "$CFG/model.json" "$HF/.local/state/opencode/model.json"
cp "$CFG/opencode.json" "$HR/.config/opencode/opencode.json"
if [ ! -f "$HF/.local/share/opencode/opencode.db" ]; then
  say "== fixture: install snapshot as freeze's native db =="
  cp "$FIX/snapshot.db" "$HF/.local/share/opencode/opencode.db"
fi
if [ ! -f "$HR/.local/share/refine/refine.db" ]; then
  say "== fixture: refine import from snapshot (equivalent data by construction) =="
  "$REFINE_BIN" import --source "$FIX/snapshot.db" --limit 1000000 \
    --data-dir "$HR/.local/share/refine" | tail -3
fi

# ---------- spawn arms ----------
PID_F=""; PID_R=""
cleanup() {
  for p in "$PID_R" "$PID_F"; do
    [ -n "$p" ] && { kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null || true; }
  done
  [ -n "${SAMPLER_PID:-}" ] && { touch "${RUN_DIR:-/nonexistent}/STOP" 2>/dev/null || true; kill "$SAMPLER_PID" 2>/dev/null || true; }
}
trap cleanup EXIT

say "== spawn (cwd=$META_CWD) =="
(
  cd "$META_CWD"
  setsid env HOME="$HF" OPENCODE_DISABLE_MODELS_FETCH=1 \
    "$REPO/$VENDOR" serve --port "$PORT_F" --hostname 127.0.0.1 >"$FIX/freeze.log" 2>&1 &
  echo $! > "$FIX/freeze.pid"
) &
(
  cd "$META_CWD"
  setsid env HOME="$HR" OPENCODE_DISABLE_MODELS_FETCH=1 \
    REFINE_DATA_DIR="$HR/.local/share/refine" REFINE_LEGACY_SYNC=0 \
    "$REPO/$REFINE_BIN" serve --port "$PORT_R" >"$FIX/refine.log" 2>&1 &
  echo $! > "$FIX/refine.pid"
) &
wait
PID_F=$(cat "$FIX/freeze.pid"); PID_R=$(cat "$FIX/refine.pid")
rm -f "$FIX/freeze.pid" "$FIX/refine.pid"

wait_healthy() { # port label
  python3 - "$1" "$2" <<'PY' || die2 "$2 never healthy on 1.18.31"
import json, sys, time, urllib.request
port, label = sys.argv[1], sys.argv[2]
for _ in range(90):
    try:
        r = urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health", timeout=2)
        v = json.loads(r.read()).get("version")
        if v != "1.18.31":
            print(f"load-test: {label} version {v} != 1.18.31", file=sys.stderr)
            sys.exit(2)
        sys.exit(0)
    except SystemExit:
        raise
    except Exception:
        time.sleep(1)
sys.exit(2)
PY
}
wait_healthy "$PORT_R" refine || { tail -20 "$FIX/refine.log" >&2; exit 2; }
wait_healthy "$PORT_F" freeze || { tail -20 "$FIX/freeze.log" >&2; exit 2; }
say "both arms healthy on 1.18.31"

# ---------- equivalence assertion (seed-parity pattern) ----------
python3 - "$PORT_R" "$PORT_F" <<'PY' || die2 "session counts differ between arms (fixture misalignment — see README levers)"
import json, sys, urllib.request
def count(port):
    d = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/session?limit=500", timeout=10))
    return len(d) if isinstance(d, list) else -1
cr, cf = count(sys.argv[1]), count(sys.argv[2])
print(f"sessions: refine={cr} freeze={cf}")
if cr != cf or cr < 1:
    sys.exit(1)
PY
say "arm equivalence ok"

# ---------- run dir + sampler ----------
TS=$(date +%Y%m%dT%H%M%SZ)
RUN_DIR="$PWD/bench/load/.runs/$TS"
mkdir -p "$RUN_DIR"
python3 - "$FIX/meta.json" "$RUN_DIR/meta.json" "$LOAD_TARGETS" "$ROUNDS" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
m["targets"] = sys.argv[3]
m["rounds"] = int(sys.argv[4])
m["order"] = ""
json.dump(m, open(sys.argv[2], "w"), indent=1)
PY
python3 bench/load/sampler.py --out "$RUN_DIR/samples.jsonl" --stop "$RUN_DIR/STOP" \
  --pids "refine=$PID_R,freeze=$PID_F" --refine-url "http://127.0.0.1:$PORT_R" &
SAMPLER_PID=$!

# ---------- k6 runs ----------
k6_flags() { # emits threshold env (GATED) or the inert-marker (baseline)
  if [ "$GATED" = 1 ]; then
    python3 - <<'PY'
import json
t = json.load(open("bench/load/thresholds.json"))
print(f"LOAD_ERR_MAX={t['err_rate_max']}")
print(f"LOAD_P95_MAX={t['p95_ms_max']}")
PY
  else
    echo "K6_NO_THRESH=1"
  fi
}

[ "$GATED" = 1 ] && [ ! -f bench/load/thresholds.json ] \
  && die2 "GATED=1 but bench/load/thresholds.json missing (run baseline first)"
set -a
eval "$(k6_flags)"
set +a

k6_run() { # $1=file $2=script $3...=env assignments (K=V)
  local out="$1" script="$2"; shift 2
  local -a envs=()
  local kv
  for kv in "$@"; do envs+=(-e "$kv"); done
  local -a flags=()
  [ "${K6_NO_THRESH:-0}" = 1 ] && flags+=(--no-thresholds)
  docker run --rm --network host \
    -v "$PWD/bench/load:/k6:ro" -v "$RUN_DIR:/out" \
    "${envs[@]}" "$K6_IMG" run "${flags[@]}" \
    --summary-export "/out/$out" "/k6/$script"
}

overall_rc=0
ORDER=""
for round in $(seq 1 "$ROUNDS"); do
  if [ $((round % 2)) -eq 1 ]; then arms="refine freeze"; else arms="freeze refine"; fi
  ORDER="$ORDER r$round:$(echo $arms | tr ' ' '-')"
  for arm in $arms; do
    if [ "$arm" = refine ]; then port=$PORT_R; else port=$PORT_F; fi
    # fault isolation: dead arm never gets averaged into fantasy numbers
    if ! python3 -c "
import sys,urllib.request
urllib.request.urlopen('http://127.0.0.1:${port}/global/health',timeout=3)" 2>/dev/null; then
      echo "INFRA: $arm arm dead before its run — aborting remaining runs" >&2
      overall_rc=2
      break 2
    fi
    for mode in spread hot; do
      say "== r$round $arm/$mode (target ladder: $LOAD_TARGETS) =="
      k6_run "r$round-$arm-$mode-readhot.summary.json" read-hot.js \
        "LOAD_BASE=http://127.0.0.1:$port" "LOAD_MODE=$mode" \
        "LOAD_SIDS=$POOL" "LOAD_DEEP=$DEEP" "LOAD_FILE_PATH=$META_CWD" \
        "LOAD_TARGETS=$LOAD_TARGETS" || {
          rc=$?
          if [ "$GATED" = 1 ]; then say "THRESHOLD BREACH (r$round $arm $mode, rc=$rc)"; overall_rc=1
          else say "k6 rc=$rc (baseline mode — thresholds inert)"; fi
        }
    done
    if [ "$LOAD_ARRIVAL" = 1 ]; then
      say "== r$round $arm/arrival (${LOAD_RPS} rps offered) =="
      k6_run "r$round-$arm-arrival-arrival.summary.json" arrival.js \
        "LOAD_BASE=http://127.0.0.1:$port" "LOAD_MODE=spread" \
        "LOAD_SIDS=$POOL" "LOAD_DEEP=$DEEP" "LOAD_FILE_PATH=$META_CWD" \
        "LOAD_RPS=$LOAD_RPS" || {
          rc=$?
          if [ "$GATED" = 1 ]; then say "THRESHOLD BREACH (arrival $arm, rc=$rc)"; overall_rc=1
          else say "k6 rc=$rc (baseline mode)"; fi
        }
    fi
  done
done
python3 -c "
import json,sys
p='$RUN_DIR/meta.json'
m=json.load(open(p)); m['order']='$ORDER'.strip(); json.dump(m,open(p,'w'),indent=1)"

# ---------- stop sampler + report ----------
touch "$RUN_DIR/STOP"
wait "$SAMPLER_PID" 2>/dev/null || true
python3 bench/load/report.py --dir "$RUN_DIR" >/dev/null
say ""
say "report: $RUN_DIR/report.md"

# ---------- isolation proof ----------
live_ok 4901 && say "live opencode :4901 healthy after run" || { echo "INFRA: live :4901 unhealthy after run" >&2; overall_rc=2; }
live_ok 4912 && say "live refine :4912 healthy after run" || { echo "INFRA: live :4912 unhealthy after run" >&2; overall_rc=2; }

if [ "$GATED" != 1 ] && [ ! -f bench/load/thresholds.json ]; then
  say ""
  say "baseline mode — next step per README: derive thresholds from this report,"
  say "commit bench/load/thresholds.json, then run GATED=1."
fi
exit "$overall_rc"
