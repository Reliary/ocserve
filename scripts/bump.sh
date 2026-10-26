#!/usr/bin/env bash
# bump.sh — mechanical version-bump pipeline (V2-PARITY-PLAN.md §6).
#
# One command per bump; everything else is gates. Monthly adoption cadence
# (nightly DETECTION via v2-drift.sh / drift-watch.sh); judgment stays human,
# enforcement stays mechanical.
#
#   scripts/bump.sh v2 2.0.32            # acquire + re-vendor + re-extract + classify
#   scripts/bump.sh v2 2.0.32 --dry-run  # print the steps, change nothing
#   scripts/bump.sh v1 1.19.3            # freeze line (acquire only; rest printed)
#
# Steps (v2 line):
#   1. acquire  — npm pack protocol/schema/cli tarballs into a STABLE scratch dir
#                 (kept across runs so step 3 can point at the CLI binary).
#   2. re-vendor — extract-spec.sh <ver> --regen (byte-reproducible contract).
#   3. extract-app — extract-app.sh <cli-binary> <ver> (web-UI pack).
#   4. classify — contract-diff.py (cross-line) + `--delta` (same-line change).
# Steps 5-9 need a running server + judgment (D-rows, allowlists, calibrations)
# and stay operator-driven; this script prints them with exact commands.
#
# This script CHANGES THE REPO TREE (steps 2-3 write the contract asset and the
# web-UI pack). To test the machinery WITHOUT adopting the bump, run it in a
# throwaway worktree: `git worktree add --detach /var/tmp/bumptest HEAD`.
#
# Env: OCSERVE_BUMP_SCRATCH (default /var/tmp/ocserve-bump-<ver>) — kept between
#      steps; `--clean` removes it. TMPDIR is forced off /tmp (often full).
set -euo pipefail
cd "$(dirname "$0")/.."

DRY=0
CLEAN=0
ARGS=()
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --clean) CLEAN=1 ;;
    *) ARGS+=("$a") ;;
  esac
done
LINE="${ARGS[0]:?usage: bump.sh <v1|v2> <version> [--dry-run] [--clean]}"
VER="${ARGS[1]:?usage: bump.sh <v1|v2> <version> [--dry-run] [--clean]}"

# /tmp is frequently full on this host; keep big scratch (200 MB CLI) on disk.
export TMPDIR="${TMPDIR:-/var/tmp}"
SCRATCH="${OCSERVE_BUMP_SCRATCH:-/var/tmp/ocserve-bump-$VER}"
run() { if [ "$DRY" = 1 ]; then echo "  [dry-run] $*"; else echo "+ $*"; "$@"; fi; }

case "$LINE" in
  v2)
    if [ "$CLEAN" = 1 ]; then rm -rf "$SCRATCH"; echo "cleaned $SCRATCH"; exit 0; fi
    echo "=== bump v2 → $VER ==="
    echo "scratch: $SCRATCH  (kept across runs; --clean to remove)"
    mkdir -p "$SCRATCH"

    echo "=== step 1: acquire (npm pack protocol + schema + cli) ==="
    echo "  (the CLI binary is ~200 MB — allow a few minutes)"
    run bash -c "cd '$SCRATCH' && npm pack @opencode/protocol@'$VER' @opencode/schema@'$VER' @opencode/cli-linux-x64@'$VER'"

    # extract the CLI binary so step 3 has a real path (the old script deleted
    # its mktemp on EXIT, leaving step 3's <cli-binary> placeholder unpointable).
    CLI="$SCRATCH/cli/package/bin/opencode"
    if [ "$DRY" = 0 ]; then
      rm -rf "$SCRATCH/cli"; mkdir -p "$SCRATCH/cli"
      tar xzf "$SCRATCH"/opencode-cli-linux-x64-*.tgz -C "$SCRATCH/cli"
      [ -x "$CLI" ] || { echo "FAIL: CLI binary not at $CLI" >&2; exit 2; }
      echo "  cli: $CLI ($(stat -c%s "$CLI") bytes)"
    fi

    echo "=== step 2: re-vendor the contract ==="
    run scripts/extract-spec.sh "$VER" --regen
    echo "  (WROTE crates/ocserve-core/assets/openapi/$VER.json — adoption artifact)"

    echo "=== step 3: re-extract the web-UI pack ==="
    run scripts/extract-app.sh "$CLI" "$VER"
    echo "  (WROTE $VER.index.bin + parts.json + ocserve-ui-* parts + manifest)"

    echo "=== step 4: classify ==="
    echo "  4a. cross-line (v1 vs new v2) against ocserve's router:"
    run python3 scripts/contract-diff.py --selftest
    run python3 scripts/contract-diff.py --v2 "$VER"
    echo "  4b. same-line delta (prev v2 → new v2) — what actually changed upstream:"
    PREV="$(grep -o 'V2_TAG: &str = "[0-9.]*"' crates/ocserve-core/src/spec.rs 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
    run python3 scripts/contract-diff.py --delta "$PREV" "$VER"

    echo
    echo "=== steps 5-9 (operator-driven) ==="
    echo "  5. regen oracle vectors — bench/permission/gen-vectors.py <freeze-url>"
    echo "     bench/permission/$VER-vectors.json (needs a running freeze; warm-up barrier)"
    echo "  6. pair + pair-v2 + replay + field-contract + permission + capdiff --full --reverse"
    echo "  7. spec-validate BOTH arms (freeze must be 0 first — K3)"
    echo "  8. event-validate, tui-check, check-guards.sh, matrix, package, publish-order"
    echo "  9. TRACEABILITY K-row + V2-PARITY-PLAN status + /api/info dark-gate re-proof"
    echo
    echo "Judgment (human, then machine-enforced): D-row naming, union-vs-reject,"
    echo "allowlist admission, relax_null calibration, /api/info release call."
    if [ "$DRY" = 1 ]; then
      echo
      echo "DRY RUN — nothing was written. Re-run without --dry-run (in a worktree"
      echo "if you do not intend to adopt) to execute steps 1-4."
    fi
    ;;
  v1)
    echo "=== bump v1 → $VER (freeze line) ==="
    echo "  scripts/acquire-freeze.sh $VER   (staged vendor binary)"
    echo "  then: replay corpus re-record (hermetic fixture), pair, spec-validate,"
    echo "  drift-watch control arm re-baseline, TRACEABILITY K-row."
    echo "  NOTE: this re-pins the FREEZE every v1 oracle is measured against —"
    echo "  run it in a worktree and adopt only after the full battery is green."
    [ "$DRY" = 1 ] || true
    ;;
  *)
    echo "usage: bump.sh <v1|v2> <version> [--dry-run] [--clean]" >&2
    exit 2
    ;;
esac
