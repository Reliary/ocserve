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

# Source resolution: a local clone with the tag is preferred (offline, fast);
# otherwise the tag's raw files are fetched over HTTPS. The local-only version
# broke the first real v1 bump (2026-11-04): the clone had v1.19.5, the caller
# passed "1.19.5", and the tag check failed on the missing "v" prefix.
have_tag() { git -C "$UPSTREAM" rev-parse -q --verify "refs/tags/$1" >/dev/null 2>&1; }
TAG_REF=""
if [ -e "$UPSTREAM/.git" ]; then
  if have_tag "$TAG"; then TAG_REF="$TAG"
  elif have_tag "v$TAG"; then TAG_REF="v$TAG"
  fi
fi
fetch_file() { # $1 = repo path, $2 = output
  if [ -n "$TAG_REF" ]; then
    git -C "$UPSTREAM" show "$TAG_REF:$1" > "$2"
  else
    curl -fsS --max-time 30 \
      "https://raw.githubusercontent.com/sst/opencode/$TAG/$1" > "$2"
  fi
}
if [ -z "$TAG_REF" ]; then
  echo "extract-prompts: tag $TAG not in $UPSTREAM — fetching raw files from GitHub"
fi

mkdir -p "$OUT"
n=0
for f in default anthropic beast gemini gpt gpt-astra kimi meta codex trinity; do
  src="packages/opencode/src/session/prompt/$f.txt"
  if fetch_file "$src" "$OUT/$f.txt" 2>/dev/null; then
    n=$((n+1))
  else
    echo "  (skip $f: not in $TAG)" >&2
    rm -f "$OUT/$f.txt"
  fi
done

# Model-facing tool descriptions (tool/*.txt) — the text bodies upstream embeds
# verbatim into each provider tool schema. Schemas themselves are zod-built in
# tool/*.ts and captured by scripts/capture-provider-contract.sh; only the
# plain-text descriptions live here.
TOUT="crates/ocserve-tools/assets/tool"
mkdir -p "$TOUT"
tn=0
for f in edit read write glob grep question todowrite webfetch websearch task skill apply_patch lsp plan-exit; do
  src="packages/opencode/src/tool/$f.txt"
  if fetch_file "$src" "$TOUT/$f.txt" 2>/dev/null; then
    tn=$((tn+1))
  else
    rm -f "$TOUT/$f.txt"
  fi
done

# The bash/shell description is NOT tool/<name>.txt upstream: the template
# lives at tool/shell/shell.txt and the command section is a TS template
# literal inside tool/shell/prompt.ts. Both were hand-vendored once (the
# capdiff commit) and therefore would have gone stale on any bump — exactly
# the hand-maintenance this script exists to remove. Extract both, and
# unescape the TS literal so the bytes match what upstream renders.
if fetch_file "packages/opencode/src/tool/shell/shell.txt" "$TOUT/shell.txt" 2>/dev/null; then
  tn=$((tn+1))
fi
_tmp_ts="$(mktemp)"
if fetch_file "packages/opencode/src/tool/shell/prompt.ts" "$_tmp_ts" 2>/dev/null; then
  python3 - "$_tmp_ts" "$TOUT/bash-command-section.txt" <<'PY_TS'
import re, sys, pathlib
src = pathlib.Path(sys.argv[1]).read_text()
m = re.search(r"function bashCommandSection\([^)]*\)\s*\{\s*return `(.*?)`\s*\}", src, re.S)
if not m:
    sys.exit("extract-prompts: bashCommandSection template not found")
body = m.group(1)
# TS template-literal escaping -> literal text (`` \` `` and `\${` only).
body = body.replace("\\`", "`").replace("\\${", "${")
pathlib.Path(sys.argv[2]).write_text(body)
PY_TS
  [ -s "$TOUT/bash-command-section.txt" ] && tn=$((tn+1))
fi
rm -f "$_tmp_ts"

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
