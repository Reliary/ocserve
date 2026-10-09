#!/usr/bin/env bash
# extract-spec.sh <tag> — vendor the OpenAPI contract from an opencode tag.
#
# The frozen 1.18.31 server ships its full OpenAPI 3.1.0 document at
# `GET /doc`; the same document lives in the upstream tree at
# `packages/sdk/openapi.json`. We vendor it verbatim so ocserve is
# self-describing and the coverage guard / validator have a stable oracle.
#
# Usage:
#   scripts/extract-spec.sh 1.18.31 [--from-live URL]
#
#   (default) reads packages/sdk/openapi.json at the given tag from the
#             local opencode worktree (OPENCODE_SRC) or a fresh shallow fetch.
#   --from-live  copies the document a running server returns at $URL/doc
#             (use to re-verify a vendor against the frozen binary).
#
# Output is pretty-printed and written to bench/openapi/<tag>.json. The file is
# a generated artifact — never hand-edit it.
set -euo pipefail

cd "$(dirname "$0")/.."
TAG="${1:?usage: extract-spec.sh <tag> [--from-live URL]}"
OUT="bench/openapi/${TAG}.json"
mkdir -p bench/openapi

if [ "${2:-}" = "--from-live" ]; then
  URL="${3:?--from-live needs a base URL, e.g. --from-live http://127.0.0.1:4901}"
  echo "fetching ${URL}/doc -> ${OUT}"
  curl -fsS --max-time 30 "${URL}/doc" | python3 -m json.tool > "${OUT}"
else
  SRC="${OPENCODE_SRC:-$HOME/src/opencode-v1}"
  if [ -d "$SRC/.git" ] && git -C "$SRC" rev-parse -q --verify "refs/tags/v${TAG}" >/dev/null 2>&1; then
    echo "reading ${SRC}@v${TAG}:packages/sdk/openapi.json -> ${OUT}"
    git -C "$SRC" show "v${TAG}:packages/sdk/openapi.json" | python3 -m json.tool > "${OUT}"
  else
    echo "no local tag v${TAG} in ${SRC}; fetching raw from GitHub"
    curl -fsS --max-time 30 \
      "https://raw.githubusercontent.com/sst/opencode/v${TAG}/packages/sdk/openapi.json" \
      | python3 -m json.tool > "${OUT}"
  fi
fi

python3 - "$OUT" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
ops = sum(len([m for m in v if m in ("get","post","put","delete","patch")]) for v in d["paths"].values())
print(f"  openapi={d.get('openapi')} paths={len(d['paths'])} ops={ops} schemas={len(d.get('components',{}).get('schemas',{}))}")
PY
echo "wrote ${OUT}"
