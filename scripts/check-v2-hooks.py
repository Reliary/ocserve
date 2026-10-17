#!/usr/bin/env python3
"""Check that every v2 plugin hook upstream declares is accounted for in
ocserve's capability matrix (V2-EXECUTION-PLAN Phase 3, item 20).

Source of truth: bench/v2/hooks.json (produced by scripts/extract-v2-hooks.sh
from the vendored upstream v2 tree). Each hook must appear in
crates/ocserve-plugin/src/host/README-v2.md — either as a bridged row or a
named divergence. A hook that is neither is an undocumented gap and fails.

This is the mechanical half of "fixture honesty": the shim's coverage claim
cannot silently drift from upstream's declared surface.
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
HOOKS = ROOT / "bench/v2/hooks.json"
MATRIX = ROOT / "crates/ocserve-plugin/src/host/README-v2.md"


def main() -> int:
    if not HOOKS.exists():
        print(f"missing {HOOKS} — run scripts/extract-v2-hooks.sh", file=sys.stderr)
        return 2
    surface = json.loads(HOOKS.read_text())
    matrix = MATRIX.read_text()
    missing = []
    total = 0
    for domain, hooks in surface.items():
        for hook in hooks:
            total += 1
            # matrix cites hooks as `session.hook("context")` or bare
            # `` `evaluate` `` etc.; accept if the hook name appears at all.
            if hook not in matrix:
                missing.append(f"{domain}.{hook}")
    if missing:
        print(
            "FAIL: v2 hooks absent from the capability matrix "
            f"(README-v2.md): {', '.join(missing)}",
            file=sys.stderr,
        )
        return 1
    print(f"ok: all {total} extracted v2 hooks are accounted for in the matrix")
    return 0


if __name__ == "__main__":
    sys.exit(main())
