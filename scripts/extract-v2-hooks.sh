#!/usr/bin/env bash
# Extract the v2 plugin hook surface from the upstream vendored v2 tree and
# emit it as JSON. Used to keep ocserve's shim honest (V2-EXECUTION-PLAN
# Phase 3, item 20): the shim must cover every hook upstream declares, or the
# gap must be a named divergence. Run when bumping the v2 pin.
#
# Usage: scripts/extract-v2-hooks.sh [V2_TREE]
#   V2_TREE defaults to $HOME/src/opencode-v2
# Output: bench/v2/hooks.json
set -euo pipefail
cd "$(dirname "$0")/.."

TREE="${1:-$HOME/src/opencode-v2}"
SRC="$TREE/packages/plugin/src/promise"
[ -d "$SRC" ] || { echo "v2 tree not found at $SRC" >&2; exit 1; }

mkdir -p bench/v2
python3 - "$SRC" > bench/v2/hooks.json <<'PY'
import re, sys, json, pathlib
src = pathlib.Path(sys.argv[1])
# Each domain file declares `interface <Domain>Hooks { readonly "name": ... }`.
domains = {"session": "SessionHooks", "tool": "ToolHooks",
           "permission": "PermissionHooks", "shell": "ShellHooks"}
out = {}
for dom, iface in domains.items():
    text = (src / f"{dom}.ts").read_text()
    m = re.search(rf"interface {iface} \{{(.*?)\n\}}", text, re.S)
    if not m:
        out[dom] = []
        continue
    body = m.group(1)
    # Top-level hook keys are indented exactly two spaces; deeper fields
    # (payload shapes inside a hook) are not hooks. Keys may be quoted
    # ("model.request") or bare (context).
    names = []
    for line in body.splitlines():
        mm = re.match(r'^  readonly\s+(?:"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))\s*:', line)
        if mm:
            names.append(mm.group(1) or mm.group(2))
    out[dom] = sorted(set(names))
print(json.dumps(out, indent=2, sort_keys=True))
PY
echo "wrote bench/v2/hooks.json"
