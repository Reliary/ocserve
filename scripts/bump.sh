#!/usr/bin/env bash
# bump.sh — mechanical version-bump pipeline (V2-PARITY-PLAN.md §6).
#
# One command per bump; everything else is gates. Monthly adoption cadence
# (nightly DETECTION via v2-drift.sh / drift-watch.sh); judgment stays human,
# enforcement stays mechanical.
#
#   scripts/bump.sh v2 2.0.32
#   scripts/bump.sh v1 1.19.3
#
# Steps (v2 line; v1 analogous via acquire-freeze.sh):
#   1. acquire: download the release artifacts (npm tarballs / binary)
#   2. re-vendor: extract-spec.sh --regen (byte-reproducible; stale-asset
#      detector = regen output must differ ONLY by version or abort)
#   3. extract-app.sh (second tag; skew guard must pass)
#   4. contract-diff.py (new/removed/conflict; STUBS need phases; conflict
#      blocks superset serving — K2, no override)
#   5. regen oracle vectors (gen-vectors.py; warm-up barrier)
#   6. full oracle battery (pair, pair-v2, replay, field-contract,
#      permission-replay, capdiff --full BOTH directions)
#   7. spec-validate both arms (freeze-0-fairness FIRST — K3)
#   8. event-validate, tui-check, guards, matrix, package, publish-order
#   9. TRACEABILITY K-row + V2-PARITY-PLAN status + /api/info dark-gate re-proof
#
# This script runs steps 1-4 (mechanical, safe) and PRINTS the step 5-9
# checklist with exact commands. Steps 5-9 need a running server + judgment
# (D-rows, allowlists, calibrations) and stay operator-driven.
set -euo pipefail
cd "$(dirname "$0")/.."

LINE="${1:?usage: bump.sh <v1|v2> <version>}"
VER="${2:?usage: bump.sh <v1|v2> <version>}"

case "$LINE" in
  v2)
    echo "=== bump v2 → $VER (step 1: acquire) ==="
    echo "(npm pack of the CLI binary is ~200 MB — allow several minutes)"
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    ( cd "$WORK" && npm pack "@opencode/protocol@${VER}" "@opencode/schema@${VER}" "@opencode/cli-linux-x64@${VER}" )
    echo "tarballs in $WORK (kept for the session; copy to scratch before exit if needed)"
    echo
    echo "=== step 2: re-vendor the contract ==="
    echo "  scripts/extract-spec.sh $VER --regen"
    echo "  (then: diff against the previous asset — version-only changes or abort)"
    echo
    echo "=== step 3: re-extract the app pack ==="
    echo "  scripts/extract-app.sh <cli-binary@$VER> $VER"
    echo "  (then: rule-17 skew guard must pass for the new tag)"
    echo
    echo "=== step 4: classify ==="
    echo "  python3 scripts/contract-diff.py --selftest && python3 scripts/contract-diff.py"
    echo "  (conflict bucket non-empty → STOP, K2; STUBS need phases)"
    echo
    echo "=== steps 5-9 (operator-driven) ==="
    echo "  5. regen oracle vectors (warm-up barrier) + pair-v2 password harness"
    echo "  6. pair + pair-v2 + replay + field-contract + permission + capdiff --full --reverse"
    echo "  7. spec-validate BOTH arms (freeze must be 0 first — K3)"
    echo "  8. event-validate, tui-check, check-guards.sh, matrix, package, publish-order"
    echo "  9. TRACEABILITY K-row + V2-PARITY-PLAN status + /api/info dark-gate re-proof"
    echo
    echo "Judgment (human, then machine-enforced): D-row naming, union-vs-reject,"
    echo "allowlist admission, relax_null calibration, /api/info release call."
    ;;
  v1)
    echo "=== bump v1 → $VER (freeze line) ==="
    echo "  scripts/acquire-freeze.sh $VER   (staged vendor binary)"
    echo "  then: replay corpus re-record (hermetic fixture), pair, spec-validate,"
    echo "  drift-watch control arm re-baseline, TRACEABILITY K-row."
    ;;
  *)
    echo "usage: bump.sh <v1|v2> <version>" >&2
    exit 2
    ;;
esac
