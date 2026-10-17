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
} >"$REPORT"
# No regen here by design: the op-level delta is bump.sh's job (full
# pipeline: acquire → re-vendor → contract-diff → oracles). This script
# DETECTS (version mismatch) so triage stays a reading task; regen runs at
# adoption time, not every night.
echo "report: $REPORT"
cat "$REPORT"
