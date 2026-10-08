#!/usr/bin/env bash
# Corpus recorder (TESTING §4 / PLAN §6): re-record the config-dependent
# manifest entries from a FROZEN upstream server running under the committed
# fixture environment (testdata/fixture), so the recorded corpus no longer
# encodes any operator's personal config (agent names, MCP servers, provider
# defaults) and the differential gate means the same thing on every machine.
#
# Usage:
#   scripts/record-corpus.sh [FREEZE_BIN] [OUT_DIR]
# Defaults: bench/parity/vendor/opencode, testdata/golden
#
# What it re-records (env-dependent routes only):
#   config, config_providers, mcp, provider, agent, command,
#   session_list, experimental_session
# Everything else (byte-mode bodies, deferred entries, notes) is preserved.
#
# Oracle rules (documented in testdata/README.md):
#   - provider: structural keys only. Per-model keys come from freeze's
#     INTERNAL registry vs ocserve's models.json catalog — recorded as
#     divergence D-PAIR-2, asserted in pair mode, not in the corpus.
#   - session_list / experimental_session: shape keys both servers emit for
#     an unprompted seeded session; agent/model/cost/summary are the
#     documented D-PAIR-3 divergence (freeze omits them before a first
#     prompt, ocserve always emits them) and are not required.
set -euo pipefail
cd "$(dirname "$0")/.."

FREEZE_BIN="${1:-bench/parity/vendor/opencode}"
OUT="${2:-testdata/golden}"
[ -x "$FREEZE_BIN" ] || { echo "record-corpus: freeze binary missing: $FREEZE_BIN" >&2; exit 2; }
[ -f "$OUT/manifest.json" ] || { echo "record-corpus: no manifest at $OUT" >&2; exit 2; }

PORT="${RECORD_PORT:-4921}"
FIXTURE="$PWD/testdata/fixture"
[ -d "$FIXTURE" ] || { echo "record-corpus: fixture missing: $FIXTURE" >&2; exit 2; }
HOME_TMP="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-record.XXXXXX")"
cp -r "$FIXTURE/." "$HOME_TMP/"

cleanup() {
  [ -n "${PID:-}" ] && { kill -- "-$PID" 2>/dev/null || kill "$PID" 2>/dev/null || true; }
  rm -rf "$HOME_TMP"
}
trap cleanup EXIT

setsid env HOME="$HOME_TMP" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$FREEZE_BIN" serve --port "$PORT" --hostname 127.0.0.1 >"$HOME_TMP/freeze.log" 2>&1 &
PID=$!
for _ in $(seq 1 90); do
  curl -fsS --max-time 1 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS --max-time 2 "http://127.0.0.1:$PORT/global/health" >/dev/null \
  || { echo "record-corpus: freeze never healthy" >&2; tail -5 "$HOME_TMP/freeze.log" >&2; exit 2; }

# Seed one session (same seed replay-check uses) so session routes answer.
curl -fsS --max-time 5 -X POST -H 'content-type: application/json' \
  -d '{"title":"OpenCode replay seed"}' "http://127.0.0.1:$PORT/session" >/dev/null

python3 - "$OUT" "$PORT" <<'PY'
import json, sys, urllib.request
out, port = sys.argv[1], sys.argv[2]
base = f"http://127.0.0.1:{port}"

RECORD = {
    "config", "config_providers", "mcp", "provider",
    "agent", "command", "session_list", "experimental_session",
}
# route-name → key filter (True = keep). Default keeps every key.
def keep_provider(k):
    return "/models." not in k  # structural only; D-PAIR-2
def keep_session(k):
    for seg in ("$.id", "$[].id", "title", "version", "directory", "time", "parentID", "slug"):
        if seg in k or k in ("$[]", "$"):
            return True
    return False
FILTERS = {
    "provider": keep_provider,
    "session_list": keep_session,
    "experimental_session": keep_session,
}

def keypaths(v, prefix="$", out=None):
    if out is None:
        out = set()
    if isinstance(v, dict):
        for k, val in v.items():
            p = f"{prefix}.{k}"
            out.add(p)
            keypaths(val, p, out)
    elif isinstance(v, list):
        p = f"{prefix}[]"
        if v:
            keypaths(v[0], p, out)
        else:
            out.add(p)
    return out

man = json.load(open(f"{out}/manifest.json"))
for name in sorted(RECORD):
    e = man[name]
    if e.get("defer"):
        continue
    req = urllib.request.urlopen(f"{base}{e['path']}", timeout=20)
    body = json.loads(req.read())
    keys = sorted(keypaths(body))
    filt = FILTERS.get(name)
    if filt:
        keys = [k for k in keys if filt(k)]
    e["keys"] = keys
    e["status"] = req.status
    if name in ("session_list", "experimental_session"):
        # Required-keys oracle: ocserve legitimately emits agent/cost/model on
        # unprompted sessions (D-PAIR-3); strict A<->B equality lives in pair mode.
        e["mode"] = "keys_subset"
    print(f"recorded {name}: {len(keys)} keys (status {req.status})")
open(f"{out}/manifest.json", "w").write(json.dumps(man, indent=1) + "\n")
PY

echo "record-corpus: wrote $OUT/manifest.json"
