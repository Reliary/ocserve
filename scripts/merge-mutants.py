#!/usr/bin/env python3
"""Merge `cargo-mutants --shard` outcome files into one combined outcomes.json.

cargo-mutants runs shards independently, each writing its own
`outcomes.json`. `check-mutants.py` expects a single file, so combine them:
sum the per-summary counts and concatenate the outcome list. Guards against
overlapping shards (a mutant id appearing twice).

Usage:
  merge-mutants.py OUT --in shard0.out/outcomes.json shard1.out/outcomes.json ...
Exit 0 on success, 1 on a duplicate mutant (shards must be disjoint).
"""
import argparse
import json
import sys
from collections import Counter
from pathlib import Path


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--in", dest="inputs", nargs="+", required=True)
    args = ap.parse_args()

    merged_outcomes: list = []
    seen: set = set()
    total = 0
    for p in args.inputs:
        d = json.loads(Path(p).read_text())
        rows = d.get("outcomes", d if isinstance(d, list) else [])
        for m in rows:
            # identity = the diff path (unique per mutant); baseline has none
            key = m.get("diff_path") or m.get("scenario") or id(m)
            if key in seen:
                print(f"FAIL: duplicate mutant across shards: {key}", file=sys.stderr)
                return 1
            seen.add(key)
            merged_outcomes.append(m)
            total += 1

    counts = Counter(m.get("summary") for m in merged_outcomes)
    combined = {
        "outcomes": merged_outcomes,
        "total_mutants": total,
        "caught": counts.get("CaughtMutant", 0),
        "missed": counts.get("MissedMutant", 0),
        "timeout": counts.get("Timeout", 0),
        "unviable": counts.get("Unviable", 0),
    }
    Path(args.out).write_text(json.dumps(combined, indent=1))
    print(
        f"merged {total} outcomes: {combined['caught']} caught, "
        f"{combined['missed']} missed, {combined['timeout']} timeout, "
        f"{combined['unviable']} unviable -> {args.out}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
