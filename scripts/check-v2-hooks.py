#!/usr/bin/env python3
"""Check that every v2 plugin hook upstream declares is accounted for in
ocserve's capability matrix (V2-EXECUTION-PLAN Phase 3, item 20).

Source of truth: bench/v2/hooks.json (produced by scripts/extract-v2-hooks.sh
from the vendored upstream v2 tree). Each hook must appear in
crates/ocserve-plugin/src/host/README-v2.md — either as a bridged row or a
named divergence. A hook that is neither is an undocumented gap and fails.

This is the mechanical half of "fixture honesty": the shim's coverage claim
cannot silently drift from upstream's declared surface.

Non-vacuity (guard rule 36): a hook cited only as a bare substring (e.g. the
word "context" appearing in prose, or "hook" matching "hooks") is NOT
accounted for — the match is anchored to the matrix's citation forms:
  - `ctx.<domain>.hook("<name>")` / `.hook("<name>", …)`
  - a markdown table cell `| <name> |` or `<name>` inside a `hook` row
and a planted missing-hook must fail (selftest below runs on every call).
"""
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
HOOKS = ROOT / "bench/v2/hooks.json"
MATRIX = ROOT / "crates/ocserve-plugin/src/host/README-v2.md"


def cited(hook: str, matrix: str) -> bool:
    # anchored citation forms only (never a bare unquoted substring match, so
    # prose like "context" or "hook" matching "hooks" does NOT count). The
    # matrix quotes hook names: `session.hook("context")` and shorthand lists
    # like `session.hook("model.request"/"http.request"/…)` — both are the
    # QUOTED form.
    patterns = [
        rf'\.hook\("{re.escape(hook)}"',
        rf'"{re.escape(hook)}"',  # quoted hook name (shorthand list items)
        rf"\|\s*{re.escape(hook)}\s*\|",  # table cell
    ]
    return any(re.search(p, matrix) for p in patterns)


def scan(surface: dict, matrix: str) -> list[str]:
    missing = []
    for domain, hooks in surface.items():
        for hook in hooks:
            if not cited(hook, matrix):
                missing.append(f"{domain}.{hook}")
    return missing


def selftest() -> bool:
    """Planted control (guard rule 36 non-vacuity): a hook that is NOT quoted
    in a matrix must be flagged. Proves the matcher bites (not vacuous)."""
    matrix = 'nothing here quotes the planted hook `model.request`\n'
    flagged = scan({"__plant__": ["model.request_unquoted_xyzzy"]}, matrix)
    if flagged != ["__plant__.model.request_unquoted_xyzzy"]:
        print("selftest: planted missing hook NOT flagged (matcher vacuous)", file=sys.stderr)
        return False
    return True


def main() -> int:
    if not HOOKS.exists():
        print(f"missing {HOOKS} — run scripts/extract-v2-hooks.sh", file=sys.stderr)
        return 2
    surface = json.loads(HOOKS.read_text())
    matrix = MATRIX.read_text()
    if not selftest():
        return 3
    missing = scan(surface, matrix)
    total = sum(len(v) for v in surface.values())
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
