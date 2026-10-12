#!/usr/bin/env bash
# extract-prompts — vendor ocserve's model-facing prompt assets from an
# upstream tag. This is the MECHANICAL bump path for the provider boundary
# (K-CAPDIFF): never hand-copy prompt text — extract it from the frozen tag
# with `git show`, so a version bump is one command + the gates.
#
#   ./scripts/extract-prompts.sh [tag]     # default: v1.18.31 (the freeze)
#
# Extracted:
#   crates/ocserve-core/assets/prompt/*.txt   agent/system prompt variants
#     (session/prompt/*.txt: default, anthropic, beast, gemini, gpt,
#      gpt-astra, kimi, meta, codex, trinity)
#
# Tool descriptions/schemas are captured from a live freeze probe by
# scripts/extract-provider-contract.sh (they are generated from zod, not
# readable as text); this script owns only the plain-text prompt files.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:-v1.18.31}"
UPSTREAM="${OCSERVE_UPSTREAM:-$HOME/src/opencode-v1}"
OUT="crates/ocserve-core/assets/prompt"

[ -e "$UPSTREAM/.git" ] || { echo "extract-prompts: no upstream clone at $UPSTREAM" >&2; exit 2; }
git -C "$UPSTREAM" rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
  || { echo "extract-prompts: tag $TAG not found" >&2; exit 2; }

mkdir -p "$OUT"
n=0
for f in default anthropic beast gemini gpt gpt-astra kimi meta codex trinity; do
  src="packages/opencode/src/session/prompt/$f.txt"
  if git -C "$UPSTREAM" cat-file -e "$TAG:$src" 2>/dev/null; then
    git -C "$UPSTREAM" show "$TAG:$src" > "$OUT/$f.txt"
    n=$((n+1))
  else
    echo "  (skip $f: not in $TAG)" >&2
  fi
done

# Model-facing tool descriptions (tool/*.txt) — the text bodies upstream embeds
# verbatim into each provider tool schema. Schemas themselves are zod-built in
# tool/*.ts and captured by scripts/capture-provider-contract.sh; only the
# plain-text descriptions live here.
TOUT="crates/ocserve-tools/assets/tool"
mkdir -p "$TOUT"
tn=0
for f in edit read write glob grep question todowrite webfetch websearch task skill apply_patch lsp plan-enter plan-exit; do
  src="packages/opencode/src/tool/$f.txt"
  if git -C "$UPSTREAM" cat-file -e "$TAG:$src" 2>/dev/null; then
    git -C "$UPSTREAM" show "$TAG:$src" > "$TOUT/$f.txt"
    tn=$((tn+1))
  fi
done

# provenance stamp so a stale asset set is detectable
{
  echo "tag: $TAG"
  echo "files: $n"
  echo "sha256:"
  (cd "$OUT" && sha256sum *.txt)
} > "$OUT/PROVENANCE"
{
  echo "tag: $TAG"
  echo "files: $tn"
  echo "sha256:"
  (cd "$TOUT" && sha256sum *.txt)
} > "$TOUT/PROVENANCE"

echo "extract-prompts: $n prompts + $tn tool descriptions from $TAG"
