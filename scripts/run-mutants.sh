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
exec cargo mutants "$@" \
  -p ocserve-store -p ocserve-core \
  --test-workspace true \
  -- -- --skip chunk_split --skip sigkill
