#!/usr/bin/env bash
# bump.sh — mechanical version-bump pipeline (V2-PARITY-PLAN.md §6).
#
# One command per bump; everything else is gates. Monthly adoption cadence
# (nightly DETECTION via v2-drift.sh / drift-watch.sh); judgment stays human,
# enforcement stays mechanical.
#
#   scripts/bump.sh v2 2.0.32            # acquire + re-vendor + re-extract + classify
#   scripts/bump.sh v2 2.0.32 --dry-run  # print the steps, change nothing
#   scripts/bump.sh v1 1.19.5            # freeze line: acquire -> contract ->
#                                        # prompts/rules -> provider capture ->
#                                        # move pin -> full battery
#
# Both lines are mechanical through the PIN MOVE; the full oracle battery
# after it is the ADOPTION gate (printed with exact commands, run by a human or
# the agent because it needs the vendored freeze and the .227 load runner).
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
    if [ "$CLEAN" = 1 ]; then rm -rf "$SCRATCH"; echo "cleaned $SCRATCH"; exit 0; fi
    echo "=== bump v1 → $VER (freeze line) ==="
    echo "scratch: $SCRATCH  (kept across runs; --clean to remove)"
    mkdir -p "$SCRATCH"
    PREV="$(sed -n 's/.*pub const FREEZE_VERSION: &str = "\([^"]*\)".*/\1/p' crates/ocserve-llm/src/lib.rs | head -1)"
    echo "current pin: ${PREV:-unreadable}"

    echo "=== step 1: acquire the freeze binary (staged vendor) ==="
    # acquire-freeze honours a positional version. It writes
    # bench/parity/vendor/opencode, which the differential harnesses resolve;
    # the update is what makes every later step measure the NEW pin instead of
    # silently re-testing the old binary.
    run scripts/acquire-freeze.sh "$VER"

    echo "=== step 2: re-vendor the v1 contract ==="
    # --from-live reads $URL/doc from the NEW freeze (byte-identical to the
    # tag's packages/sdk/openapi.json), which also proves the staged binary
    # actually boots. Slot output (v1.json) either way.
    BOOT_PORT="${BUMP_FREEZE_PORT:-4951}"
    if [ "$DRY" = 0 ] && [ -x bench/parity/vendor/opencode ]; then
      BOOT_HOME="$(mktemp -d "$SCRATCH/home.XXXXXX")"
      mkdir -p "$BOOT_HOME/.config/opencode" "$BOOT_HOME/.local/share/opencode" \
               "$BOOT_HOME/.local/state/opencode"
      for f in opencode.json auth.json model.json; do
        case "$f" in
          opencode.json) dst="$BOOT_HOME/.config/opencode/$f" ;;
          auth.json)     dst="$BOOT_HOME/.local/share/opencode/$f" ;;
          model.json)    dst="$BOOT_HOME/.local/state/opencode/$f" ;;
        esac
        [ -f "bench/parity/config/$f" ] && cp "bench/parity/config/$f" "$dst"
      done
      ( HOME="$BOOT_HOME" setsid bench/parity/vendor/opencode serve \
          --port "$BOOT_PORT" --hostname 127.0.0.1 >"$SCRATCH/freeze.log" 2>&1 & echo $! >"$SCRATCH/freeze.pid" )
      ok=0
      for _ in $(seq 1 60); do
        if curl -fsS -m 2 "http://127.0.0.1:$BOOT_PORT/global/health" >/dev/null 2>&1; then ok=1; break; fi
        sleep 1
      done
      if [ "$ok" != 1 ]; then
        tail -5 "$SCRATCH/freeze.log" >&2 || true
        kill "$(cat "$SCRATCH/freeze.pid")" 2>/dev/null || true
        echo "FAIL: staged freeze $VER did not become healthy on :$BOOT_PORT" >&2
        exit 2
      fi
      run scripts/extract-spec.sh "$VER" --from-live "http://127.0.0.1:$BOOT_PORT"
      kill "$(cat "$SCRATCH/freeze.pid")" 2>/dev/null || true
    else
      echo "  [dry-run] scripts/extract-spec.sh $VER --from-live http://127.0.0.1:$BOOT_PORT"
    fi

    echo "=== step 3: re-vendor prompt assets + pure-data tables ==="
    run scripts/extract-prompts.sh "$VER"
    run scripts/extract-rules.sh "$VER"

    echo "=== step 4: provider-boundary capture (needs a booted freeze) ==="
    if [ "$DRY" = 0 ] && [ -x bench/parity/vendor/opencode ]; then
      run scripts/capture-provider-contract.sh
    else
      echo "  [dry-run] scripts/capture-provider-contract.sh"
    fi

    echo "=== step 5: move the pin ==="
    # The pin is the LAST mechanical edit: every artifact above was extracted
    # from $VER, and this line is what makes the server report it.
    if [ "$DRY" = 1 ]; then
      echo "  [dry-run] set FREEZE_VERSION = $VER in crates/ocserve-llm/src/lib.rs"
    else
      PREV="$PREV" VER="$VER" python3 - <<'PYEOF'
import os, pathlib, re, sys
p = pathlib.Path("crates/ocserve-llm/src/lib.rs")
s = p.read_text()
old, new = os.environ["PREV"], os.environ["VER"]
pat = re.compile(r'(pub const FREEZE_VERSION: &str = ")[^"]*(";)')
s2, n = pat.subn(rf'\g<1>{new}\g<2>', s, count=1)
if n != 1:
    sys.exit("FAIL: FREEZE_VERSION const not found exactly once")
p.write_text(s2)
print(f"  pin: {old} -> {new}")
PYEOF
      grep -q "pub const FREEZE_VERSION: &str = \"$VER\";" crates/ocserve-llm/src/lib.rs \
        || { echo "FAIL: pin edit did not apply" >&2; exit 2; }
    fi

    echo
    echo "=== step 6: full battery (the ADOPTION gate) ==="
    echo "  cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings"
    echo "  cargo test --workspace"
    echo "  ./scripts/check-guards.sh && ./scripts/check-matrix.sh && ./scripts/check-docs.py"
    echo "  OCSERVE_CORPUS=testdata/fixture ./scripts/replay-check.sh   # re-record first:"
    echo "    ./scripts/record-corpus.sh  (hermetic fixture HOME; the recorded corpus"
    echo "     encodes env-dependent values — 2026-11-04 paired-check finding)"
    echo "  ./scripts/pair-check.sh && ./scripts/capdiff-check.sh --selftest --full"
    echo "  ./scripts/permission-check.sh && ./scripts/field-contract-check.sh"
    echo "  ./scripts/hook-contract-check.sh"
    echo "  ./scripts/spec-validate-v2.sh  (v1/spec-validate via pair's spec mode)"
    echo "  G2 load thresholds re-derive on .227 (arrival runs stay separate)"
    echo
    echo "Judgment: corpus re-record diff, G2 bounds, TRACEABILITY K-<version> row."
    echo "Adopt only when the whole battery is green; revert = one commit."
    ;;
  *)
    echo "usage: bump.sh <v1|v2> <version> [--dry-run] [--clean]" >&2
    exit 2
    ;;
esac
