#!/usr/bin/env python3
"""Guard rule 29 — mutating-endpoint effects classification (SNAPSHOT.md §8).

The class this closes: a mutating endpoint that returns a success *shape*
without performing the mutation (`/vcs/apply` returned a fabricated `not-clean`
for years; `revert` wrote an inert marker). Route-binding (rule 16) and
response-shape (P4) guards structurally cannot see it — only an assertion on the
*observable effect* (worktree/file bytes, DB rows) can.

`bench/sidefx/effects.tsv` classifies every mutating operation in the frozen
OpenAPI contract:

  tested:<crate/tests/file.rs>  a real side-effect test references this route
  no-effect:<reason>            the route has no persistent server/worktree
                                mutation to assert (transport/control, or a
                                feature ocserve does not perform)

This checker enforces that the classification is COMPLETE and CONSISTENT:
  - every mutating op in the spec appears exactly once,
  - no row references a path/op absent from the spec,
  - every `tested:` citation resolves to an existing test file that actually
    contains the route path (so the effect test cannot be silently deleted),
  - every `no-effect:` reason is non-empty.

Exit 0 when consistent, 1 otherwise. `--selftest` plants a violation (a
`tested:` row pointing at a missing file, and a missing op) and requires the
SAME comparisons to flag it — non-vacuous by construction.
"""
import json
import sys
from pathlib import Path

SPEC = "crates/ocserve-core/assets/openapi/1.18.31.json"
EFFECTS = "bench/sidefx/effects.tsv"
MUTATING = ("post", "put", "patch", "delete")


def spec_ops():
    d = json.load(open(SPEC))
    ops = set()
    for path, methods in d["paths"].items():
        for m in methods:
            if m in MUTATING:
                ops.add((m.upper(), path))
    return ops


def rows():
    out = []
    for line in open(EFFECTS):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            out.append(("MALFORMED", line, "", ""))
            continue
        out.append(tuple(parts))
    return out


def segs(p):
    return [x for x in p.split("/") if x]


def covers(route, url):
    """Route template covers a concrete URL (segment-wise, `{param}` wildcard)."""
    r, u = segs(route), segs(url)
    if len(r) != len(u):
        return False
    for a, b in zip(r, u):
        if a.startswith("{") and a.endswith("}"):
            continue
        if a != b:
            return False
    return True


def file_references(path, text):
    """True iff `text` contains a quoted URL literal that `path` covers."""
    import re
    for m in re.finditer(r'"(/[A-Za-z0-9_\-/{}.]+)"', text):
        if covers(path, m.group(1)):
            return True
    return False


def check(ops, rs, tests_dir=Path("crates")):
    errs = []
    seen = set()
    for method, path, _oid, status in rs:
        if method == "MALFORMED":
            errs.append(f"malformed row: {path!r}")
            continue
        key = (method, path)
        if key in seen:
            errs.append(f"duplicate row: {method} {path}")
        seen.add(key)
        if key not in ops:
            errs.append(f"row for an op absent from the spec: {method} {path}")
        if status.startswith("tested:"):
            ref = status[len("tested:"):]
            f = Path(ref)
            if not f.is_file():
                errs.append(f"tested citation does not exist: {ref} (for {method} {path})")
            elif not file_references(path, f.read_text()):
                errs.append(f"tested citation {ref} does not drive route {path}")
        elif status.startswith("no-effect:"):
            if not status[len("no-effect:"):].strip():
                errs.append(f"empty no-effect reason: {method} {path}")
        else:
            errs.append(f"bad classification {status!r}: {method} {path}")
    missing = ops - {k for k in seen if k in ops}
    for m, p in sorted(missing):
        errs.append(f"mutating op not classified: {m} {p}")
    return errs


def main():
    args = sys.argv[1:]
    if "--selftest" in args:
        ops = spec_ops()
        rs = rows()
        base = check(ops, rs)
        if base:
            print("selftest baseline is not clean:", base[:3])
            return 1
        # plant 1: a tested: row pointing at a missing file
        planted = rs + [("POST", "/session/{sessionID}/revert", "x", "tested:crates/nope/missing.rs")]
        if not check(ops, planted):
            print("FAIL: planted missing-test-file citation not detected")
            return 1
        # plant 2: drop an op so it becomes unclassified
        dropped = rs[:-1]
        if not check(ops, dropped):
            # only a violation if the dropped row was a spec op
            tail = rs[-1]
            if (tail[0], tail[1]) in ops:
                print("FAIL: planted unclassified op not detected")
                return 1
        # plant 3: a row for an op absent from the spec
        phantom = rs + [("POST", "/nonexistent/route", "x", "no-effect:phantom")]
        if not check(ops, phantom):
            print("FAIL: planted phantom op not detected")
            return 1
        print("ok (all three planted violations detected)")
        return 0

    ops = spec_ops()
    rs = rows()
    errs = check(ops, rs)
    if errs:
        for e in errs:
            print("FAIL:", e)
        return 1
    tested = sum(1 for r in rs if r[3].startswith("tested:"))
    print(f"ok ({len(rs)} mutating ops classified; {tested} effect-tested; "
          f"{len(rs) - tested} no-effect)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
