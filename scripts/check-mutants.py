#!/usr/bin/env python3
"""Mutation-score gate (TESTING §1 rule 1).

Reads `cargo mutants` output (`mutants.out/outcomes.json`) and fails when the
number of MISSED mutants exceeds the agreed ceiling. The threshold is agreed
at the first measured baseline, then ratcheted down — never up without a
recorded reason (same discipline as the test-count ratchet).

Excluded from scoring, with reasons (these are not test failures):
  - Unviable: the mutant does not compile (mutating an operator in a way the
    type system rejects) — no test can observe it.
  - Timeout: surfaced separately; a hang is its own bug class and is reported
    but not counted as a survivor.

Usage:
  check-mutants.py [--baseline <n>] [--json <path>]
    --baseline  missed-mutant ceiling (default: the agreed value below)
Exit 0 within budget, 1 over budget, 2 no results.
"""
import argparse
import json
import sys
from pathlib import Path

# Agreed at the first measured baseline (2026-10-17); ratchet DOWN over time.
# Ratcheting up requires a commit message naming the reason.
DEFAULT_MISSED_CEILING = 400


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", type=int, default=DEFAULT_MISSED_CEILING)
    ap.add_argument("--json", default="mutants.out/outcomes.json")
    args = ap.parse_args()

    p = Path(args.json)
    if not p.exists():
        print(f"no mutant results at {p} — run `cargo mutants` first", file=sys.stderr)
        return 2
    data = json.loads(p.read_text())
    missed = data.get("missed", 0)
    caught = data.get("caught", 0)
    timeout = data.get("timeout", 0)
    total = data.get("total_mutants", missed + caught)
    scored = missed + caught
    score = (caught / scored * 100) if scored else 0.0
    print(
        f"mutants: {scored} scored ({caught} caught / {missed} missed), "
        f"{timeout} timeout, {total} total — mutation score {score:.1f}%"
    )
    if missed > args.baseline:
        print(
            f"FAIL: {missed} missed mutants exceeds ceiling {args.baseline} "
            f"(ratchet down only; raise requires a recorded reason)",
            file=sys.stderr,
        )
        return 1
    print(f"ok: {missed} missed <= ceiling {args.baseline}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
