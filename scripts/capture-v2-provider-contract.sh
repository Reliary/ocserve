#!/usr/bin/env bash
# capture-v2-provider-contract — re-capture the v2 model-facing tool schemas
# from the pinned OpenCode v2 binary's own HTTP surface.
#
#   ./scripts/capture-v2-provider-contract.sh [path-to-v2-binary]
#
# Boots the binary twice under a throwaway HOME with a local OpenAI-compatible
# sink, once with a plain model (edit+write family) and once with a `gpt-5`
# model (patch family), and writes the union to
# crates/ocserve-tools/assets/provider-tools-v2.json. Never hand-edit that file.
#
# Named limits: `todowrite`, artifact, schedule and opencode-namespace tools are
# release-flag gated and off in a default capture; the asset is the default
# surface. Code Mode `execute` is captured but not advertised by ocserve.
set -euo pipefail
cd "$(dirname "$0")/.."

V2BIN="${1:-${V2_BIN:-$(command -v opencode)}}"
[ -x "$V2BIN" ] || { echo "capture-v2: binary not found: $V2BIN" >&2; exit 2; }
ver=$("$V2BIN" --version 2>/dev/null | head -1)
OUT="crates/ocserve-tools/assets/provider-tools-v2.json"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-v2-capture.XXXXXX")
SINKPID=""
cleanup() { [ -n "$SINKPID" ] && kill "$SINKPID" 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT

capture_one() { # $1 = model id, $2 = port, $3 = sink port, $4 = out dir
  local model="$1" port="$2" sink="$3" dir="$4"
  mkdir -p "$dir/home/.config/opencode" "$dir/home/.local/share/opencode" \
           "$dir/home/.local/state/opencode" "$dir/home/.cache/opencode" "$dir/req"
  python3 bench/capdiff/sink.py "$sink" "$dir/req" >/dev/null 2>&1 &
  SINKPID=$!
  sleep 1
  cat > "$dir/home/.config/opencode/opencode.json" <<JSON
{"\$schema":"https://opencode.ai/config.json","model":"stub/$model",
 "providers":{"stub":{"name":"Capture","package":"@ai-sdk/openai-compatible",
  "settings":{"baseURL":"http://127.0.0.1:$sink/v1","apiKey":"capture-key"},
  "models":{"$model":{"name":"Capture","modelID":"$model","limit":{"context":200000,"output":8192}}}}},
 "plugins":[],"mcp":{"servers":{}}}
JSON
  OPENCODE_CONFIG_DIR="$dir/home/.config/opencode" HOME="$dir/home" \
    OPENCODE_DISABLE_MODELS_FETCH=1 timeout 120 "$V2BIN" run --standalone \
    --model "stub/$model" "capture" >"$dir/run.log" 2>&1 \
    || { echo "capture-v2: run failed for $model" >&2; tail -5 "$dir/run.log" >&2; exit 1; }
  kill "$SINKPID" 2>/dev/null || true
  SINKPID=""
}

capture_one "deepseek-v4" 4961 8218 "$TMP/plain"
capture_one "gpt-5" 4962 8219 "$TMP/gpt"

python3 - "$TMP" "$OUT" "$ver" <<'PY'
import glob, json, pathlib, sys
tmp, out, ver = sys.argv[1], sys.argv[2], sys.argv[3]
tools = {}
for pattern in (f"{tmp}/plain/req/*.json", f"{tmp}/gpt/req/*.json"):
    for f in sorted(glob.glob(pattern)):
        body = json.load(open(f)).get("body") or {}
        for tool in body.get("tools") or []:
            tools.setdefault(tool["function"]["name"], tool)
if not tools:
    sys.exit("capture-v2: no tools-bearing request captured")
order = ["shell", "read", "write", "edit", "patch", "glob", "grep", "question",
         "skill", "subagent", "todowrite", "webfetch", "websearch",
         "experimental_image_generate", "execute"]
ordered = [tools[n] for n in order if n in tools]
ordered += [tools[n] for n in tools if n not in order]
pathlib.Path(out).write_text(json.dumps(ordered, indent=2) + "\n")
prov = pathlib.Path(out).with_suffix("").with_suffix(".PROVENANCE")
prov.write_text(
    f"source: live OpenCode {ver} provider request (two captures: plain model, gpt-5 for the patch family)\n"
    f"tools: {len(ordered)}\n"
    "rebuild: ./scripts/capture-v2-provider-contract.sh — never hand-edit\n"
    "note: todowrite/artifact/schedule/opencode-namespace tools are release-flag gated and absent by default.\n")
print(f"captured {len(ordered)} v2 tool schemas from {ver} -> {out}")
PY
