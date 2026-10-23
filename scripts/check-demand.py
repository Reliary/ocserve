#!/usr/bin/env python3
"""Rule 50: the demand ledger must match the corpus and classify every hook.

Re-derives status from bench/plugins/report.json (the measured input) and
compares it to the COMMITTED ledger. A generator that writes both artefacts
would make the guard vacuous; this reads the input independently.
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

REPORT = json.loads((ROOT / "bench/plugins/report.json").read_text())
LEDGER = json.loads((ROOT / "bench/plugins/demand.json").read_text())

declared = {}
for r in REPORT["results"]:
    for hook in r.get("hooks") or []:
        declared.setdefault(hook, []).append(r["name"])
missing = sorted(set(declared) - set(LEDGER))
extra = sorted(set(LEDGER) - set(declared))
if missing:
    print("hooks present in the corpus but missing from the ledger:", missing)
if extra:
    print("hooks in the ledger that the corpus no longer declares:", extra)
bad = [h for h, r in LEDGER.items() if r.get("status") not in ("bound", "diverged")]
if bad:
    print("unclassified:", bad)
sys.exit(1 if (missing or extra or bad) else 0)
