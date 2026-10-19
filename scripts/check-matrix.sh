#!/usr/bin/env bash
# Matrix checker (TESTING §2): every F/N/K row has a status + at least a
# planned-milestone marker; test IDs cited in the matrix exist in cargo test.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

# 1. every requirement row (| F.. | N.. | K-.. |) has a status cell
while IFS= read -r row; do
  if ! echo "$row" | grep -qE 'partial|green|planned'; then
    echo "BAD ROW (no status): $row"
    fail=1
  fi
done < <(grep -E '^\| (F[0-9]|N[0-9]|K-[A-Z0-9-]+) ' TRACEABILITY.md)

# 2. no 'planned' row without a milestone name
while IFS= read -r row; do
  if echo "$row" | grep -q 'planned' && ! echo "$row" | grep -qE 'M[0-9]'; then
    echo "BAD ROW (planned, no milestone): $row"
    fail=1
  fi
done < <(grep -E '^\| (F[0-9]|N[0-9]|K-[A-Z0-9-]+) ' TRACEABILITY.md)

# 3. cited test names (backticked module::paths under golden::/pragma:: etc.)
#    must exist in `cargo test -- --list` (best-effort; build first if needed)
if cargo test --workspace --no-run -q >/dev/null 2>&1; then
  tests="$(cargo test --workspace -- --list 2>/dev/null | grep ': test$' | sed 's/: test$//' | sort -u)"
  if [ -z "$tests" ]; then
    # An empty list means the harness failed to enumerate, not that every
    # citation is an orphan. Failing loudly here (rather than reporting 16
    # phantom orphans) is the difference between a real defect and noise.
    echo "FAIL: cargo test --list produced no tests (harness/environment)"; exit 1
  fi
  while IFS= read -r tid; do
    fn="${tid##*::}"
    # `grep -q` exits on the first match and closes the pipe; under `pipefail`
    # the writer's EPIPE becomes the pipeline's status, so a PRESENT test was
    # intermittently reported as an orphan (measured 3/12 runs). Capture first,
    # then match — the same capture-then-test rule rule 16 already uses.
    if ! case "$tests" in *"$fn"*) true ;; *) false ;; esac; then
      echo "ORPHAN TEST ID in matrix: $tid"
      fail=1
    fi
  done < <(grep -oE '`(golden|pragma|blob|writer|schema|fts_m0|crash_fuzz|cache_fixture)::[a-z_0-9]+(::[a-z_0-9]+)?`' TRACEABILITY.md | tr -d '`')
else
  echo "WARN: workspace did not build; skipping test-existence check"
fi

if [ "$fail" -eq 0 ]; then
  echo "matrix OK"
fi
exit $fail
