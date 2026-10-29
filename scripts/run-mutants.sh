#!/usr/bin/env bash
# Run cargo-mutants with the ONLY honest scope.
#
# cargo-mutants defaults to running just the MUTATED PACKAGE's tests. That is
# wrong for this workspace: `ocserve-store` functions are largely covered by
# `ocserve-http`'s integration tests (the fork/revert routes), so the package
# default reports them as survivors.
#
# Measured 2026-10-28:
#   fork.rs    package scope  0% (0 caught / 26 missed)
#              workspace scope 77% (23 caught / 3 missed)
#   revert.rs  package scope 45% (9 caught / 17 missed)
#
# The 2026-10-17 baseline (43.5%, 1221 missed) was produced with the package
# default, so its per-module 0% rows are an artifact, not a coverage gap. Its
# aggregate ceiling is therefore LOOSE, not tight: it was ratcheted from a
# number that over-counted survivors.
#
# Usage: scripts/run-mutants.sh [extra cargo-mutants args...]
set -euo pipefail
cd "$(dirname "$0")/.."

# `-i/--in-place` edits the SOURCE TREE and restores it when each mutant is
# done. A killed run (timeout, Ctrl-C, OOM, a rebooting box) therefore leaves a
# mutant on disk, and the next `git commit` would ship it. Caught on
# 2026-10-29: after a killed calibration, `crates/ocserve-core/src/ids.rs` was
# still mutated. Refuse to start on a dirty tree, and require the caller to
# clean up after an interrupted in-place run rather than discovering it later.
for arg in "$@"; do
  case "$arg" in
    -i|--in-place)
      if [ -n "$(git status --porcelain)" ]; then
        echo "run-mutants.sh: --in-place needs a clean tree (a killed run leaves a mutant behind)" >&2
        echo "  fix: git checkout -- .   # after confirming you have no real edits" >&2
        exit 2
      fi
      ;;
  esac
done

# The two known-slow harness tests are skipped under mutation:
#   chunk_split  — 50 s seeded fuzz, unrelated to a mutant's behaviour
#   sigkill      — kills a child at a RANDOM offset, so it fails for reasons
#                  that have nothing to do with the mutant (this is exactly how
#                  it masked a real non-atomic-migration bug on 2026-10-28; that
#                  class is now pinned by tests/mig_atomicity.rs instead)
#
# CONFIG TEST-ARGS ARE APPENDED AFTER THE `--` SEPARATOR. `.cargo/mutants.toml`
# documents this for `additional_cargo_test_args`, and the same trap applies to
# anything a caller passes through: `"$@"` after `--` becomes a TEST BINARY
# argument. Passing `--output /tmp/x` that way produced
#   error: Unrecognized option: 'output'
# and cargo-mutants then aborted with "cargo test failed in an unmutated tree"
# — a confusing diagnosis for a wrapper bug (2026-10-29, first remote attempt).
# So: cargo-mutants' own options go BEFORE the separator, and the test-arg
# passthrough stays fixed and last.
# `--test-workspace true` re-runs the WHOLE workspace test build per mutant,
# which is correct but expensive: each job compiles every crate and links every
# integration binary. Two consequences worth knowing before setting jobs:
#   * per-mutant cost is ~40 s at -j1 (measured), so the full 2707-mutant run is
#     ~30 CPU-hours; it is a nightly/weekly job, never a commit gate;
#   * jobs multiply that rebuild, not just the test time. `-j16` on a 32-thread
#     box drove load past 200 and made no faster progress than `-j8`
#     (2026-10-29). Eight is the measured sweet spot on this tree.
#
# `--in-place` avoids copying the repo (and, on the laptop, an 801 GB target
# tree that a copy would try to duplicate). It restores the source itself; a
# killed run can leave a mutant on disk, so check `git status` after an abort.
exec cargo mutants "$@" \
  -p ocserve-store -p ocserve-core \
  --test-workspace true \
  -- -- --skip chunk_split --skip sigkill
