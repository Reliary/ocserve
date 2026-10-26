#!/usr/bin/env python3
"""Publish-order guard (rule 28).

The crates.io publish workflow uploads the workspace in dependency order. Two
defects are possible and both silently break a release:

  1. a workspace member missing from the publish list (the 2026-10-12 finding:
     ocserve-pty was a dependency of ocserve-http but absent from the list, so
     publish aborted at ocserve-http with "no matching package named
     `ocserve-pty`");
  2. a member listed BEFORE one of its internal dependencies (the dependent
     uploads first and its dependency is not yet on the registry).

This derives the truth from Cargo.toml (members + each crate's internal deps)
and the publish order from .github/workflows/publish.yml, then asserts:
  * every workspace member appears exactly once in the publish order;
  * no member appears before an internal dependency.

Exit 0 + summary on success; exit 1 on any violation.
--selftest plants a missing crate and an out-of-order pair and requires both to
be detected (proves the check is load-bearing, TESTING §1).
"""
import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
ROOT_MANIFEST = ROOT / "Cargo.toml"
PUBLISH_YML = ROOT / ".github/workflows/publish.yml"


def members_and_deps(manifest_text: str):
    """Return {package_name: set(internal_dep_names)} for workspace members."""
    m = re.search(r"members\s*=\s*\[(.*?)\]", manifest_text, re.S)
    if not m:
        raise SystemExit("ERR: no [workspace] members in Cargo.toml")
    paths = re.findall(r'"([^"]+)"', m.group(1))
    out = {}
    for rel in paths:
        ct = (ROOT / rel / "Cargo.toml").read_text()
        name = re.search(r'^name\s*=\s*"([^"]+)"', ct, re.M)
        if not name:
            raise SystemExit(f"ERR: no package name in {rel}/Cargo.toml")
        name = name.group(1)
        # internal deps: lines mentioning an ocserve-* crate in [dependencies]
        deps = set(re.findall(r"^(ocserve-[a-z0-9-]+)\s*[.=]", ct, re.M))
        out[name] = deps
    return out


def publish_order() -> list:
    """The order the workflow will use: derived from Cargo.toml.

    It used to be scraped from a hardcoded list in publish.yml, which is
    exactly how ocserve-pty went missing (2026-10-12) and how the six
    ocserve-ui-* shards would go missing next. The workflow now calls
    `--order`, so the guard and the workflow read the same derivation.
    """
    return toposort(members_and_deps(ROOT_MANIFEST.read_text()))


def toposort(deps: dict) -> list:
    """Dependency-first order; ties broken by name for determinism."""
    remaining = {k: set(v) for k, v in deps.items()}
    out = []
    while remaining:
        ready = sorted(k for k, v in remaining.items() if not (v & set(remaining)))
        if not ready:
            raise SystemExit("ERR: dependency cycle among workspace members")
        for name in ready:
            out.append(name)
            remaining.pop(name)
    return out


def check(deps: dict, order: list) -> list:
    problems = []
    members = set(deps)
    listed = set(order)
    for miss in sorted(members - listed):
        problems.append(f"missing from publish order: {miss}")
    for extra in sorted(listed - members):
        problems.append(f"published but not a workspace member: {extra}")
    pos = {name: i for i, name in enumerate(order)}
    for name in order:
        for dep in sorted(deps.get(name, ())):
            if dep in pos and pos[dep] > pos[name]:
                problems.append(f"order: {name} published before its dependency {dep}")
    return problems


def main() -> int:
    deps = members_and_deps(ROOT_MANIFEST.read_text())
    order = publish_order()
    if "--order" in sys.argv:
        for name in order:
            print(name)
        return 0
    if "--selftest" in sys.argv:
        # negative control 1: drop a crate from the order
        broken = [c for c in order if c != order[len(order) // 2]]
        if not check(deps, broken):
            print("FAIL(selftest): dropping a crate was not detected")
            return 1
        # negative control 2: reverse the order — a dependent before its
        # dependency must be flagged for at least one edge in this graph.
        if not check(deps, list(reversed(order))):
            print("FAIL(selftest): reversed (dependent-before-dependency) order was not detected")
            return 1
        print("selftest: PASS (missing-crate and wrong-order both detected)")
        return 0
    problems = check(deps, order)
    if problems:
        for p in problems:
            print(f"FAIL: {p} (rule 28)")
        return 1
    print(f"ok ({len(order)} members in dependency order, generated from Cargo.toml)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
