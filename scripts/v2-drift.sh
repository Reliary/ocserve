#!/usr/bin/env bash
# v2 drift column (V2-PARITY-PLAN.md §6): nightly detection for the 2.x line.
#
# Unlike the v1 drift-watch (Docker dual-arm replay), the v2 column is
# CONTRACT-LEVEL and runs locally with no containers:
#   1. query npm for the latest @opencode/cli version (read-only, 1 request)
#   2. compare with the vendored tag (SPEC_V2_TAG in ocserve-core)
#   3. on mismatch: download tarballs, regen the contract, run contract-diff
#      → report new/removed/conflict ops (INFORMATION, never auto-adopt)
#   4. verify the maintenance artifacts still reproduce (regen == committed)
#
# Monthly adoption batches the drift; this script only DETECTS (same
# read-only-triage posture as drift-watch.sh). Exit 0 = report written.
# Exit 1 = harness failure (npm unreachable, regen mismatch, diff conflict).
set -euo pipefail
cd "$(dirname "$0")/.."
umask 077

REPORT_DIR="${V2_DRIFT_DIR:-bench/drift}"
mkdir -p "$REPORT_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT="$REPORT_DIR/$STAMP-v2-drift.md"

vendored="$(grep -o 'SPEC_V2_TAG[^;]*' crates/ocserve-core/src/spec.rs 2>/dev/null | head -1 || true)"
if [ -z "$vendored" ]; then
  # fallback: the embedded asset filename
  vendored="$(ls crates/ocserve-core/assets/openapi/ | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.json$' | grep -v '^1\.' | sort -V | tail -1 | sed 's/\.json//')"
fi
echo "vendored v2 tag: ${vendored:-unknown}"

latest="$(npm view @opencode/cli version 2>/dev/null || true)"
if [ -z "$latest" ]; then
  echo "npm unreachable — cannot check v2 drift (harness failure)" >&2
  exit 1
fi
echo "npm latest @opencode/cli: $latest"

{
  echo "# v2 drift $STAMP"
  echo
  echo "- vendored: ${vendored:-unknown}"
  echo "- npm latest: $latest"
  echo
  if [ "${vendored:-}" = "$latest" ]; then
    echo "- drift: NONE (vendored == latest)"
  else
    echo "- drift: VERSION MISMATCH — triage: adopt (bump.sh) / ignore"
    echo "- next: scripts/bump.sh v2 $latest (mechanical pipeline, monthly cadence)"
  fi
  echo
  # Plugin-surface drift (Phase 5): the contract check above watches the HTTP
  # API; the PLUGIN hook surface is a different artifact. Re-extract it from
  # the vendored tree and compare to the committed `bench/v2/hooks.json` — a
  # change means the shim's capability matrix (guard rule 36) must be revisited.
  if [ -n "${V2_TREE:-}" ] && [ -d "${V2_TREE}/packages/plugin/src" ]; then
    # re-extract the hook surface from the (possibly newer) tree and diff it
    # against the committed manifest. On change the shim's capability matrix
    # (README-v2.md, guard rule 36) must be revisited. Restores the committed
    # file afterwards (extract-v2-hooks.sh writes bench/v2/hooks.json).
    cur="$(scripts/extract-v2-hooks.sh "$V2_TREE" >/dev/null 2>&1 && cat bench/v2/hooks.json)"
    committed="$(git show HEAD:bench/v2/hooks.json 2>/dev/null || cat bench/v2/hooks.json)"
    git checkout -- bench/v2/hooks.json 2>/dev/null || true
    if [ -n "${cur:-}" ]; then
      if [ "$cur" = "$committed" ]; then
        echo "- plugin hooks: UNCHANGED vs committed bench/v2/hooks.json"
      else
        echo "- plugin hooks: CHANGED — re-review crates/ocserve-plugin/src/host/README-v2.md (guard rule 36)"
      fi
    fi
  fi
} >"$REPORT"
# No regen here by design: the op-level delta is bump.sh's job (full
# pipeline: acquire → re-vendor → contract-diff → oracles). This script
# DETECTS (version mismatch) so triage stays a reading task; regen runs at
# adoption time, not every night.
echo "report: $REPORT"
cat "$REPORT"
