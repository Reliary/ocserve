#!/usr/bin/env bash
# capture-provider-contract — re-capture the frozen provider tool schemas from
# the vendored freeze binary's own HTTP interface. This is the MECHANICAL bump
# path for the provider boundary (K-CAPDIFF): never hand-edit tool schemas.
#
#   ./scripts/capture-provider-contract.sh
#
# Boots freeze under a throwaway fixture HOME with the provider `stub` pointed
# at a local capture sink, sends one prompt, and writes the captured `tools`
# array to crates/ocserve-tools/assets/provider-tools.json. Then the human runs
# the gates (capdiff must stay GREEN).
set -euo pipefail
cd "$(dirname "$0")/.."

VENDOR="${OCSERVE_FREEZE_BIN:-bench/parity/vendor/opencode}"
[ -x "$VENDOR" ] || { echo "capture-provider-contract: vendored freeze missing: $VENDOR" >&2; exit 2; }
ver=$("$VENDOR" --version 2>/dev/null | head -1)

PORT=4934
SINK=8215
OUT="crates/ocserve-tools/assets/provider-tools.json"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-capture.XXXXXX")
trap 'rm -rf "$TMP"; kill $SINKPID $FPID 2>/dev/null || true' EXIT

python3 "$(dirname "$0")/../bench/capdiff/sink.py" "$SINK" "$TMP" >/dev/null 2>&1 &
SINKPID=$!
sleep 1

mkdir -p "$TMP/home/.config/opencode" "$TMP/home/.local/share/opencode" \
         "$TMP/home/.local/state/opencode" "$TMP/home/.cache/opencode"
cat > "$TMP/home/.config/opencode/opencode.json" <<JSON
{"\$schema":"https://opencode.ai/config.json","model":"stub/stub-1",
 "provider":{"stub":{"name":"Capture","npm":"@ai-sdk/openai-compatible",
  "options":{"baseURL":"http://127.0.0.1:$SINK/v1","apiKey":"capture-key"},
  "models":{"stub-1":{"name":"Stub One","limit":{"context":200000,"output":8192}}}}},
 "plugin":[],"mcp":{}}
JSON
printf '{"stub":{"type":"api","key":"capture-key"}}' > "$TMP/home/.local/share/opencode/auth.json"
cp "$HOME/.cache/opencode/models.json" "$TMP/home/.cache/opencode/models.json" 2>/dev/null || true

setsid env HOME="$TMP/home" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$VENDOR" serve --port "$PORT" --hostname 127.0.0.1 >"$TMP/freeze.log" 2>&1 &
FPID=$!
for _ in $(seq 1 40); do sleep 0.5; curl -s "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break; done
SID=$(curl -s -X POST "http://127.0.0.1:$PORT/session?directory=%2F" \
  -H 'content-type: application/json' -d '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
curl -s -X POST "http://127.0.0.1:$PORT/session/$SID/message" \
  -H 'content-type: application/json' \
  -d '{"model":{"providerID":"stub","modelID":"stub-1"},"parts":[{"type":"text","text":"capture"}]}' >/dev/null
for _ in $(seq 1 40); do sleep 0.5; ls "$TMP"/req-*.json >/dev/null 2>&1 && break; done

python3 - "$TMP" "$OUT" "$ver" <<'PY'
import json, glob, sys, pathlib
tmp, out, ver = sys.argv[1], sys.argv[2], sys.argv[3]
tools = None
for f in sorted(glob.glob(f"{tmp}/req-*.json")):
    b = json.load(open(f))["body"]
    if b.get("tools"):
        tools = b["tools"]
if not tools:
    sys.exit("capture: no tools-bearing request captured")
pathlib.Path(out).write_text(json.dumps(tools, indent=2) + "\n")
prov = pathlib.Path(out).with_suffix("").with_suffix(".PROVENANCE")
prov.write_text(
    f"source: live freeze {ver} provider request (scripts/capture-provider-contract.sh)\n"
    f"tools: {len(tools)}\n"
    f"rebuild: ./scripts/capture-provider-contract.sh — never hand-edit\n")
print(f"captured {len(tools)} tool schemas from {ver} -> {out}")
PY
