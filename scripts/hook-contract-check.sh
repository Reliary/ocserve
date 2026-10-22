#!/usr/bin/env bash
# Hook-contract gate: capture the reference surface and ours, diff them.
#
#   ./scripts/hook-contract-check.sh              # capture + diff (needs the freeze)
#   ./scripts/hook-contract-check.sh --self-test  # prove the comparator is not vacuous
#
# Requires the vendored freeze binary (bench/parity/vendor/opencode); CI runners
# do not have it (the same constraint as pair-check/capdiff), so this runs in
# nightly and locally. The static guard (rule 47) is the CI-side proxy.
set -uo pipefail
cd "$(dirname "$0")/.."

FREEZE="bench/parity/vendor/opencode"
OCSERVE_BIN="${OCSERVE_BIN:-target/release/ocserve}"
ALLOW="bench/hooks/allow.txt"

if [ "${1:-}" = "--self-test" ]; then
  # 1. the comparator must catch a real shape difference
  python3 - <<'PY'
import json, sys, importlib.util
spec = importlib.util.spec_from_file_location("n", "bench/hooks/normalise.py")
n = importlib.util.module_from_spec(spec); spec.loader.exec_module(n)
a = {"surface": {"h": {"input": {"model": ["object"]}, "output": {}}}, "markers": {"s": ["h"]}}
b = {"surface": {"h": {"input": {"model": ["string"]}, "output": {}}}, "markers": {"s": ["h"]}}
f = n.diff(a, b, [])
assert any("kind reference=object candidate=string" in x for x in f), f
# 2. a missing trigger must be caught
b2 = {"surface": {}, "markers": {}}
f2 = n.diff(a, b2, [])
assert any(x.startswith("missing trigger") for x in f2), f2
# 3. allowlist filtering must be exact-token, not a substring accident
assert not n.diff(a, b, ["kind reference=object candidate=string"]), "allow did not filter"
print("selftest: kind change, missing trigger, and allow filtering all behave")
PY
  [ $? -eq 0 ] || { echo "hook-contract-check: SELFTEST FAILED" >&2; exit 1; }
  exit 0
fi

[ -x "$FREEZE" ] || { echo "hook-contract-check: vendored freeze missing ($FREEZE)" >&2; exit 2; }
[ -x "$OCSERVE_BIN" ] || { echo "hook-contract-check: $OCSERVE_BIN missing (cargo build --release)" >&2; exit 2; }

set -e
./scripts/capture-hooks.sh >/dev/null
OCSERVE_HOOKS_BIN="$OCSERVE_BIN" ./scripts/capture-hooks.sh >/dev/null
set +e

python3 bench/hooks/normalise.py surface bench/hooks/freeze.jsonl bench/hooks/freeze.surface.json >/dev/null
python3 bench/hooks/normalise.py surface bench/hooks/ocserve.jsonl bench/hooks/ocserve.surface.json >/dev/null
python3 bench/hooks/normalise.py diff bench/hooks/freeze.surface.json \
  bench/hooks/ocserve.surface.json "$ALLOW"
# Intra-request order is an invariant, not a per-scenario sequence: within each
# request round the reference fires system.transform -> chat.params -> chat.headers
# (request.ts:70/115/135). The reference dispatches each hook twice (double
# registration in this build), so consecutive duplicates are collapsed first.
python3 bench/hooks/order_check.py bench/hooks/freeze.jsonl bench/hooks/ocserve.jsonl
rc=$?

# Effect observables: run the ACTIVE probe on both arms. A hook dispatched with
# the right shape and then ignored passes every payload comparison; this is the
# only check that fails for that class.
if [ $rc -eq 0 ]; then
  HOOKS_PROBE=active ./scripts/capture-hooks.sh 2>&1 | grep -E '^effects|EFFECT' || rc=1
  HOOKS_PROBE=active OCSERVE_HOOKS_BIN="$OCSERVE_BIN" ./scripts/capture-hooks.sh 2>&1 | grep -E '^effects|EFFECT' || rc=1
fi
exit $rc
