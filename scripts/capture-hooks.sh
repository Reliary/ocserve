#!/usr/bin/env bash
# hook-contract capture: record every v1 plugin trigger the REFERENCE
# implementation fires, with its real input/output payloads.
#
#   ./scripts/capture-hooks.sh                    # from the vendored freeze
#   OCSERVE_HOOKS_BIN=target/release/ocserve ./scripts/capture-hooks.sh
#
# Why capture instead of reading the .d.ts: types cannot tell you which hooks are
# actually fired, in what order, or which optional keys are present. The reference
# is the oracle; the type declarations are only an enumeration.
#
# Output: bench/hooks/<tag>.jsonl             raw trigger log
#         bench/hooks/<tag>.scenarios.json    what ran + exit status
#         bench/hooks/<tag>.surface.json      normalised key surface per trigger
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${OCSERVE_HOOKS_BIN:-}"
if [ -z "$BIN" ]; then
  BIN="bench/parity/vendor/opencode"
  TAG="${HOOKS_TAG:-freeze}"
else
  TAG="${HOOKS_TAG:-ocserve}"
fi
[ -x "$BIN" ] || { echo "capture-hooks: binary not executable: $BIN" >&2; exit 2; }

PORT="${HOOKS_PORT:-4936}"
STUB_PORT="${HOOKS_STUB_PORT:-18096}"
OUT="bench/hooks/${TAG}.jsonl"
SCEN="bench/hooks/${TAG}.scenarios.json"
SURF="bench/hooks/${TAG}.surface.json"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-hooks.XXXXXX")"
export HOOKS_LOG="$TMP/hooks.jsonl"
# effect observables for the active probe
SHELLOUT="$TMP/shell.out"
export SHELLOUT
export HOOKS_TAG="$TAG"
: >"$HOOKS_LOG"

cleanup() {
  [ -n "${STUBPID:-}" ] && kill "$STUBPID" 2>/dev/null
  if [ -n "${SRVPID:-}" ]; then
    kill -- -"$SRVPID" 2>/dev/null || kill "$SRVPID" 2>/dev/null
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---- deterministic provider (emits exactly one tool call per turn) ---------
STUB_PORT="$STUB_PORT" python3 bench/hooks/toolstub.py >"$TMP/stub.log" 2>&1 &
STUBPID=$!
for _ in $(seq 1 40); do
  curl -sf -m 2 "http://127.0.0.1:$STUB_PORT/_requests" >/dev/null 2>&1 && break
  sleep 0.25
done

# ---- fixture HOME -----------------------------------------------------------
mkdir -p "$TMP/home/.config/opencode" "$TMP/home/.local/share/opencode" \
         "$TMP/home/.local/state/opencode" "$TMP/home/.cache/opencode" "$TMP/work"
# HOOKS_PROBE=active selects the mutating probe (proves CONSUMPTION, not just
# dispatch); the passive probe is the default because a mutating probe changes
# the very payload it records.
case "${HOOKS_PROBE:-passive}" in
  active) PROBE="$(cd bench/hooks && pwd)/probe-active.mjs" ;;
  *)      PROBE="$(cd bench/hooks && pwd)/probe.mjs" ;;
esac
cat >"$TMP/home/.config/opencode/opencode.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "model": "stub/stub-1",
  "provider": {
    "stub": {
      "name": "Hook Stub",
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:$STUB_PORT/v1", "apiKey": "hook-key" },
      "models": {
        "stub-1": { "name": "Stub One", "limit": { "context": 200000, "output": 8192 } }
      }
    }
  },
  "plugin": ["$PROBE"],
  "mcp": {}
}
JSON
printf '{"stub":{"type":"api","key":"hook-key"}}' >"$TMP/home/.local/share/opencode/auth.json"
printf '{"recent":[{"providerID":"stub","modelID":"stub-1"}],"favorite":[],"variant":{}}' \
  >"$TMP/home/.local/state/opencode/model.json"
cp "$HOME/.cache/opencode/models.json" "$TMP/home/.cache/opencode/models.json" 2>/dev/null || true

# ---- boot ------------------------------------------------------------------
echo "capture-hooks: $TAG from $BIN (port $PORT, stub $STUB_PORT)"
setsid env HOME="$TMP/home" OPENCODE_DISABLE_MODELS_FETCH=1 \
  OCSERVE_DATA_DIR="$TMP/home/.local/share/opencode" \
  OCSERVE_LEGACY_SYNC=0 OCSERVE_CONFIG_WATCH=0 \
  "$BIN" serve --port "$PORT" --hostname 127.0.0.1 >"$TMP/server.log" 2>&1 &
SRVPID=$!
ready=0
for _ in $(seq 1 160); do
  if curl -sf -m 2 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1; then ready=1; break; fi
  kill -0 "$SRVPID" 2>/dev/null || break
  sleep 0.25
done
if [ "$ready" != 1 ]; then
  echo "capture-hooks: server did not become healthy" >&2
  tail -30 "$TMP/server.log" >&2
  exit 2
fi

results="[]"
note() {
  results=$(python3 -c '
import json,sys
r=json.loads(sys.argv[1]); r.append({"scenario":sys.argv[2],"ok":sys.argv[3]=="0","detail":sys.argv[4][:300]})
print(json.dumps(r))' "$results" "$1" "$2" "$3")
}
# request with a hard timeout so a bad stub can never wedge the harness
req() { curl -s -m 90 "$@"; }

# Scenario boundary marker in the trigger log. Without it the corpus has no way
# to compare ORDER: a hook that fires at the wrong point in the turn is a real
# divergence (plugins read evolving state), and the first version of this
# harness silently compared nothing because no marker existed.
mark() { printf '{"marker":"%s"}\n' "$1" >>"$HOOKS_LOG"; }

mk_session() {
  req -X POST "http://127.0.0.1:$PORT/session?directory=%2F" \
    -H 'content-type: application/json' -d '{}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))'
}

# ---- S1: prompt with one tool round ----------------------------------------
# Covers chat.message, chat.params, chat.headers, system.transform,
# messages.transform, tool.definition, shell.env, tool.execute.before/after.
mark prompt
SID=$(mk_session)
[ -n "$SID" ] || { echo "capture-hooks: no session id" >&2; exit 2; }
req -X POST "http://127.0.0.1:$PORT/session/$SID/message" \
  -H 'content-type: application/json' \
  -d '{"model":{"providerID":"stub","modelID":"stub-1"},"parts":[{"type":"text","text":"hook capture prompt"}]}' \
  >"$TMP/s1.json"
note prompt "$?" "$SID"

# ---- S2: a user shell call (shell.env without a model) ---------------------
mark shell
SID2=$(mk_session)
req -X POST "http://127.0.0.1:$PORT/session/$SID2/shell" \
  -H 'content-type: application/json' \
  -d '{"agent":"build","command":"echo hook-shell","model":null}' >"$TMP/s2.json"
note shell "$?" "$SID2"

# ---- S3: a command (command.execute.before) --------------------------------
# Use a BUILT-IN command: freeze resolves config-defined commands lazily through
# its own command registry, and a fixture-defined one failed there (UnknownError
# with an empty stack). `init` is always present, so the call site is reachable.
mark command
SID3=$(mk_session)
req -X POST "http://127.0.0.1:$PORT/session/$SID3/command" \
  -H 'content-type: application/json' \
  -d '{"command":"init","arguments":"","agent":"build","model":"stub/stub-1","parts":[]}' \
  >"$TMP/s3.json"
note command "$?" "$SID3"

# ---- S4: summarize (session.compacting + messages.transform) --------------
mark summarize
SID4=$(mk_session)
req -X POST "http://127.0.0.1:$PORT/session/$SID4/message" \
  -H 'content-type: application/json' \
  -d '{"model":{"providerID":"stub","modelID":"stub-1"},"parts":[{"type":"text","text":"first turn"}]}' \
  >/dev/null
req -X POST "http://127.0.0.1:$PORT/session/$SID4/message" \
  -H 'content-type: application/json' \
  -d '{"model":{"providerID":"stub","modelID":"stub-1"},"parts":[{"type":"text","text":"second turn"}]}' \
  >/dev/null
req -X POST "http://127.0.0.1:$PORT/session/$SID4/summarize" \
  -H 'content-type: application/json' -d '{"providerID":"stub","modelID":"stub-1"}' \
  >"$TMP/s4.json"
note summarize "$?" "$SID4"

# ---- S5: effect observables (active probe only) ---------------------------
# A hook can be dispatched with the right shape and still be IGNORED. These
# checks look at where a mutation LANDS, not at the hook call itself.
if [ "${HOOKS_PROBE:-passive}" = "active" ]; then
  req -X POST "http://127.0.0.1:$PORT/session/$SID/shell" \
    -H 'content-type: application/json' \
    -d '{"agent":"build","command":"printf %s $HOOKPROBE_SHELL_ENV","model":null}' \
    >"$TMP/s5.json"
  python3 bench/hooks/effects.py "$TMP" "$TMP/s5.json" "$STUB_PORT"
fi

# ---- collect ---------------------------------------------------------------
cp "$HOOKS_LOG" "$OUT" 2>/dev/null || : >"$OUT"
python3 - "$SCEN" "$results" <<'PY'
import json, sys, pathlib
pathlib.Path(sys.argv[1]).write_text(json.dumps(json.loads(sys.argv[2]), indent=2) + "\n")
PY

python3 - "$OUT" "$SURF" <<'PY'
import json, sys, pathlib, collections
events = []
for line in open(sys.argv[1]):
    try:
        events.append(json.loads(line))
    except Exception:
        pass
order = [e["hook"] for e in events if "hook" in e]
surface = {}
for hook in dict.fromkeys(order):
    ins, outs = set(), set()
    for e in events:
        if e.get("hook") != hook:
            continue
        i, o = e.get("input"), e.get("output")
        if isinstance(i, dict):
            ins |= set(i)
        if isinstance(o, dict):
            outs |= set(o)
    surface[hook] = {"input": sorted(ins), "output": sorted(outs)}
doc = {
    "sdk_note": "payload surface captured from the reference implementation, not inferred from types",
    "records": len(events),
    "sequence": order,
    "surface": surface,
}
pathlib.Path(sys.argv[2]).write_text(json.dumps(doc, indent=2) + "\n")
print("captured", len(events), "records,", len(surface), "hooks")
print("sequence:", " -> ".join(dict.fromkeys(order)))
PY
