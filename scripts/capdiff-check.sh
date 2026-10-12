#!/usr/bin/env bash
# capdiff-check — thin wrapper: asserts the release binary + vendored freeze
# exist, then runs bench/capdiff/capdiff.py. Kept as a script (not inline) so
# guard rule 27 and nightly share one entrypoint and the freeze binary check
# lives in one place.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -x target/release/ocserve ] || { echo "capdiff-check: build release first" >&2; exit 2; }
[ -x bench/parity/vendor/opencode ] || { echo "capdiff-check: vendored freeze missing" >&2; exit 2; }
exec python3 bench/capdiff/capdiff.py "$@"
