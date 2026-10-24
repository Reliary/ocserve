#!/usr/bin/env python3
"""Validate the committed tool-level ask recording (guard rule 56).

The recording is the oracle `bench/permission/tool-oracle.py` diffs against.
If it is empty, truncated, or missing the fields that matter, the differential
would pass while testing nothing — the exact vacuity class this repo has hit
before (rule 47's comparator, rule 27's plant).

Standalone file rather than an inline heredoc so it can be run and sabotaged
directly: `check-recording.py <recording.json>`.

Exit 0 = usable, 1 = vacuous/malformed.
"""
import json
import sys


def main(path):
    try:
        doc = json.load(open(path))
    except Exception as e:
        print(f"recording unreadable: {e}")
        return 1
    cases = doc.get("cases")
    if not isinstance(cases, dict) or len(cases) < 10:
        print(f"recording missing or too few cases ({len(cases) if isinstance(cases, dict) else 0})")
        return 1
    asks = 0
    for name, entry in cases.items():
        rows = entry.get("asks") or []
        if not rows:
            print(f"case recorded no asks (probe never reached it): {name}")
            return 1
        asks += len(rows)
        for a in rows:
            if not a.get("permission"):
                print(f"{name}: ask has no permission")
                return 1
            if "patterns" not in a or "always" not in a:
                print(f"{name}: ask missing patterns/always")
                return 1
            if a["permission"] in ("edit", "write"):
                keys = a.get("metadata_keys") or []
                if "diff" not in keys:
                    print(f"{name}: edit/write ask has no diff metadata ({keys})")
                    return 1
                if not (a.get("diff") or "").startswith("Index: "):
                    print(f"{name}: edit/write diff is not a rendered patch")
                    return 1
    if asks < 10:
        print(f"recording has {asks} asks across {len(cases)} cases (vacuous)")
        return 1
    print(f"ok ({asks} asks across {len(cases)} cases)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else
                  "bench/permission/1.18.31.tool-asks.json"))
