#!/usr/bin/env python3
"""Validate the capdiff scenario matrix (guard rule 27): non-empty, unique
names, non-empty steps. Exit 0 + one line on success, exit 1 on any defect."""
import json, pathlib, sys

p = pathlib.Path(__file__).with_name("scenarios.json")
d = json.loads(p.read_text())
scs = d.get("scenarios", [])
if not scs:
    sys.exit("ERR: no scenarios")
names = [s.get("name") for s in scs]
if len(names) != len(set(names)):
    sys.exit(f"ERR: duplicate scenario names {names}")
for s in scs:
    if not s.get("steps"):
        sys.exit(f"ERR: scenario {s.get('name')} has no steps")
    for st in s["steps"]:
        if st.get("type") not in ("prompt", "summarize"):
            sys.exit(f"ERR: scenario {s['name']} bad step {st}")
print(f"ok ({len(scs)} scenarios: {', '.join(names)})")
