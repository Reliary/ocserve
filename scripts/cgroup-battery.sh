#!/usr/bin/env bash
# Child-containment battery (SRE §5.2, guard rule 45). Local-only: the cgroup
# dance requires a delegated systemd tree, which CI runners do not have.
#
# Proves the 2026-10-21 class is closed: a tool child must live in
# kids/<leaf>, not in main/ with the server, and a backgrounded child must
# outlive the tool call while still being capped.
#
# Method: boot ocserve inside a transient systemd user scope (so it gets
# INVOCATION_ID and the dance runs), drive the REAL /shell route (no model),
# then read /proc/<pid>/cgroup for the process the shell started.
#
# Negatives are structural, not decorative:
#   A. a plain child must report kids/tools            (plant: seam disabled)
#   B. `sleep 300 &` must survive the tool call AND still be in kids/tools
#   C. the server itself must report main/              (plant: self-move removed)
#   D. leaves must carry oom.group=1 and the aggregate must NOT
#
# Usage: scripts/cgroup-battery.sh [--self-test]
# Exit 0 = all assertions pass; 1 = failure; 2 = setup.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
PORT="${CG_PORT:-4942}"
UNIT="ocserve-cgbattery-$$"
SELF_TEST="${1:-}"

die() { echo "cgroup-battery: $*" >&2; exit 2; }
log() { echo "[$(date +%H:%M:%S)] $*"; }

[ -x "$BIN" ] || die "$BIN missing (cargo build --release)"
command -v systemd-run >/dev/null || die "systemd-run missing"
# "degraded" still means the manager is up (some unrelated unit failed);
# only offline/unknown means there is nothing to delegate a scope to.
state=$(systemctl --user is-system-running 2>/dev/null || true)
case "$state" in
  running|degraded|starting|initializing|maintenance) ;;
  *) die "no usable user systemd (state: ${state:-unknown})" ;;
esac

if [ "$SELF_TEST" = "--self-test" ]; then
  python3 - <<'PY'
import sys
# the assertion the battery makes, exercised against known-good/bad paths
def classified(path, leaf):
    return path.rstrip('/').endswith('/main') if leaf == 'main' else path.rstrip('/').endswith('/kids/' + leaf)
assert classified('0::/x/ocserve.service/main', 'main')
assert not classified('0::/x/ocserve.service', 'main')
assert classified('0::/x/ocserve.service/kids/tools', 'tools')
assert not classified('0::/x/ocserve.service/kids/mcp', 'tools')
assert not classified('0::/x/ocserve.service/main', 'tools')
print('selftest: cgroup path classifier distinguishes main / kids/<leaf> / neither')
PY
  [ $? -eq 0 ] || exit 1
  exit 0
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cgbattery.XXXXXX")" || die "mktemp"
cleanup() {
  systemctl --user stop "$UNIT" >/dev/null 2>&1
  systemctl --user reset-failed "$UNIT" >/dev/null 2>&1
  # the transient scope's cgroup disappears with the unit; kill any stray
  # sleeper we started so a failed run cannot leak a process for 5 minutes
  if [ -n "${SLEEP_PID:-}" ]; then kill -9 "$SLEEP_PID" 2>/dev/null; fi
  # leaf cgroups can outlive the unit if something reparented; nothing here
  # survives the unit's cgroup removal, so no extra sweep is needed.
  rm -rf "$TMP"
}
trap cleanup EXIT

# A bare config: no plugins, no MCP, no model — /shell needs none of them.
mkdir -p "$TMP/home/.config/opencode" "$TMP/data"
cat > "$TMP/home/.config/opencode/opencode.json" <<'JSON'
{"$schema":"https://opencode.ai/config.json","plugin":[],"mcp":{}}
JSON

log "booting $BIN in $UNIT (port $PORT)"
# Transient SERVICE, not --scope: user scopes reject OOMScoreAdjust
# ("Unknown assignment"), and a service is also what production runs, so the
# dance is exercised under the same unit semantics.
systemd-run --user --unit="$UNIT" --collect \
  -p MemoryMax=1024M -p MemorySwapMax=0 \
  -p MemoryAccounting=yes -p OOMScoreAdjust=200 \
  -p StandardOutput=file:"$TMP/server.log" \
  -p StandardError=append:"$TMP/server.log" \
  -E HOME="$TMP/home" -E OCSERVE_DATA_DIR="$TMP/data" \
  -E OCSERVE_LEGACY_SYNC=0 -E OCSERVE_CONFIG_WATCH=0 \
  "$BIN" serve --port "$PORT" --hostname 127.0.0.1 >/dev/null 2>&1 \
  || { tail -20 "$TMP/server.log" 2>/dev/null; die "systemd-run failed"; }

for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 \
  || { tail -20 "$TMP/server.log"; die "server did not become healthy"; }

SID=$(curl -s -X POST "http://127.0.0.1:$PORT/session" \
  -H 'content-type: application/json' \
  -d '{"title":"cgroup battery"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
[ -n "$SID" ] || die "no session id"

RUN_PID=$(systemctl --user show "$UNIT" -p MainPID --value)
case "$RUN_PID" in ''|0) die "unit $UNIT has no MainPID";; esac
CG="$(cat /proc/"$RUN_PID"/cgroup 2>/dev/null | sed 's/^0:://' || true)"
[ -n "$CG" ] || die "cannot resolve the server pid"

pass=0; failn=0
ok()  { echo "  PASS: $*"; pass=$((pass+1)); }
bad() { echo "  FAIL: $*"; failn=$((failn+1)); }
cgroup_of() { cat "/proc/$1/cgroup" 2>/dev/null | sed 's/^0:://'; }

# --- C: the server itself is in main/ ---------------------------------------
case "$CG" in
  */main) ok "server is in main/ ($CG)" ;;
  *)      bad "server is NOT in main/ ($CG) — partition self-move did not run" ;;
esac

# --- D: leaves carry oom.group; the aggregate does not ----------------------
ROOT="/sys/fs/cgroup${CG%/main}"
if [ -d "$ROOT/kids" ]; then
  for leaf in plugin mcp tools pty; do
    if [ -d "$ROOT/kids/$leaf" ]; then
      [ "$(cat "$ROOT/kids/$leaf/memory.oom.group" 2>/dev/null)" = "1" ] \
        && ok "kids/$leaf oom.group=1" \
        || bad "kids/$leaf oom.group != 1"
    else
      bad "kids/$leaf missing (the child would be uncontained)"
    fi
  done
  [ "$(cat "$ROOT/kids/memory.oom.group" 2>/dev/null)" = "1" ] \
    && bad "kids/ carries oom.group=1 (a build would kill the browser host)" \
    || ok "aggregate kids/ is not an oom group (value 0)"
else
  bad "no kids/ directory at $ROOT"
fi

# --- A: a normal tool child lands in kids/tools -----------------------------
# Fire and poll: /shell BLOCKS until the command exits, so the child only
# exists while the request is in flight.
curl -s -X POST "http://127.0.0.1:$PORT/session/$SID/shell" \
  -H 'content-type: application/json' \
  -d '{"agent":"build","command":"sleep 8","model":null}' >/dev/null &
REQ_PID=$!

# Find a process by exact command line, restricted to THIS unit's cgroup tree
# so a coincidental `sleep 8` elsewhere on the box can never satisfy the
# assertion (that would be a vacuous pass).
find_in_unit() {
  local pat="$1" p
  for p in $(pgrep -f "$pat" 2>/dev/null); do
    case "$(cat "/proc/$p/cgroup" 2>/dev/null)" in
      *"$UNIT.service"*) echo "$p"; return 0 ;;
    esac
  done
  return 1
}
CHILD=""
for _ in $(seq 1 80); do
  CHILD=$(find_in_unit '^sleep 8$') && break
  sleep 0.1
done
CHILD_CG=""
CHILD_ADJ=""
if [ -n "$CHILD" ]; then
  CHILD_CG="$(cgroup_of "$CHILD")"
  CHILD_ADJ="$(cat "/proc/$CHILD/oom_score_adj" 2>/dev/null)"
fi
wait "$REQ_PID" 2>/dev/null
if [ -z "$CHILD" ]; then
  bad "could not observe the tool child (is /shell running a shell?)"
else
  CCG="$CHILD_CG"
  case "$CCG" in
    */kids/tools) ok "tool child contained in kids/tools ($CHILD)" ;;
    */main)       bad "tool child is in main/ ($CHILD) — the 2026-10-21 class is OPEN" ;;
    "")           bad "tool child $CHILD vanished before it could be classified" ;;
    *)            bad "tool child in unexpected cgroup: $CCG" ;;
  esac
  [ "$CHILD_ADJ" = "500" ] \
    && ok "tool child adj=500 (server is 200)" \
    || bad "tool child adj=$CHILD_ADJ != 500"
  [ "$(cat "/proc/$RUN_PID/oom_score_adj" 2>/dev/null)" = "200" ] \
    && ok "server adj=200 (pinned in the unit, not a distro default)" \
    || bad "server adj=$(cat "/proc/$RUN_PID/oom_score_adj" 2>/dev/null) != 200"
fi

# --- B: a backgrounded child outlives the tool call, still contained --------
# The demonstrated bug: `nohup ... &` outlives the tool call. The tool returns
# immediately; the child is reparented but MUST stay in the class leaf.
curl -s -X POST "http://127.0.0.1:$PORT/session/$SID/shell" \
  -H 'content-type: application/json' \
  -d '{"agent":"build","command":"nohup sleep 300 >/dev/null 2>&1 &","model":null}' \
  >/dev/null
sleep 1
SLEEP_PID=$(find_in_unit '^sleep 300$') || SLEEP_PID=""
if [ -z "$SLEEP_PID" ]; then
  bad "backgrounded child did not survive the tool call (test cannot verify containment)"
else
  S_CG="$(cgroup_of "$SLEEP_PID")"
  case "$S_CG" in
    */kids/tools) ok "backgrounded child survived AND is contained ($SLEEP_PID)" ;;
    */main)       bad "backgrounded child survived in main/ ($SLEEP_PID) — placement failed" ;;
    *)            bad "backgrounded child in unexpected cgroup: $S_CG" ;;
  esac
fi

echo
log "result: $pass passed, $failn failed"
[ "$failn" -eq 0 ] || exit 1
exit 0
