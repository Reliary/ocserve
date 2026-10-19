#!/usr/bin/env python3
"""Rule 39 — a script that offers `--selftest` must have a gate that RUNS it.

A selftest nothing invokes is the registered-but-never-called class this repo
already shipped once (the v1 `config` plugin hook returned a hard-coded "ok"
and was never dispatched). `fetch-release.sh` shipped with a real, passing
selftest that no gate called, so a future rot would have stayed invisible.

An "acceptor" is a shell/python script that implements the `--selftest` flag
(not merely calls another script's). Each acceptor must be named TOGETHER WITH
`--selftest` in at least one gate file: scripts/check-guards.sh,
scripts/nightly.sh, a GitHub workflow, or a wrapper that a gate runs.

--selftest plants an orphaned acceptor in a temp dir and requires the check
to flag it; a check that cannot see a fresh orphan is vacuous.
"""
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GATE_FILES = [
    "scripts/check-guards.sh",
    "scripts/nightly.sh",
    "bench/snapshot-it/run.sh",
]
GATE_GLOBS = [".github/workflows"]


def candidates(root: str) -> list[str]:
    out: list[str] = []
    for base in ("scripts", "bench"):
        for dirpath, dirnames, filenames in os.walk(os.path.join(root, base)):
            dirnames[:] = [d for d in dirnames if d != "__pycache__"]
            for name in filenames:
                if not name.endswith((".sh", ".py")):
                    continue
                out.append(os.path.relpath(os.path.join(dirpath, name), root))
    return sorted(out)


def accepts_selftest(path: str) -> bool:
    """The file IMPLEMENTS --selftest (owns the flag), rather than calling it."""
    try:
        text = open(path, encoding="utf-8", errors="ignore").read()
    except OSError:
        return False
    impl = [
        r"--selftest\)",                      # bash case arm
        r'["\']--selftest["\']\s+in\s',       # python argv membership
        r'\bSELFTEST\s*=\s*1',                # bash flag var
    ]
    return any(re.search(p, text) for p in impl)


def gate_text(root: str) -> str:
    chunks: list[str] = []
    for rel in GATE_FILES:
        p = os.path.join(root, rel)
        if os.path.isfile(p):
            chunks.append(open(p, encoding="utf-8", errors="ignore").read())
    import glob

    for pattern in GATE_GLOBS:
        for p in glob.glob(os.path.join(root, pattern, "*.yml")):
            chunks.append(open(p, encoding="utf-8", errors="ignore").read())
    return "\n".join(chunks)


def check(root: str) -> list[str]:
    gates = gate_text(root)
    orphans: list[str] = []
    for rel in candidates(root):
        path = os.path.join(root, rel)
        if not accepts_selftest(path):
            continue
        base = os.path.basename(rel)
        stem = base.rsplit(".", 1)[0]
        # Named together with --selftest somewhere in the gate surface. Docker
        # compose services drop the extension (`run --rm differential --selftest`),
        # so the stem counts as a name too.
        names = sorted({base, stem}, key=len, reverse=True)
        pat = re.compile(
            "|".join(
                re.escape(n) + r"[^\n]*--selftest|--selftest[^\n]*" + re.escape(n)
                for n in names
            )
        )
        if base == "check-guards.sh":
            # It IS the gate; its own selftest is exercised by CI.
            continue
        if not pat.search(gates):
            orphans.append(rel)
    return orphans


def selftest() -> int:
    with tempfile.TemporaryDirectory() as d:
        os.makedirs(os.path.join(d, "scripts"))
        os.makedirs(os.path.join(d, "bench"))
        # an acceptor no gate names
        open(os.path.join(d, "scripts", "orphan_tool.sh"), "w").write(
            "#!/bin/sh\ncase \"$1\" in --selftest) echo ok;; esac\n"
        )
        open(os.path.join(d, "scripts", "check-guards.sh"), "w").write(
            "#!/bin/sh\necho nothing\n"
        )
        found = check(d)
        if found != ["scripts/orphan_tool.sh"]:
            print(f"FAIL selftest: orphan not detected (got {found})")
            return 1
        # now the gate names it: must clear
        open(os.path.join(d, "scripts", "check-guards.sh"), "a").write(
            "\n./scripts/orphan_tool.sh --selftest\n"
        )
        if check(d):
            print("FAIL selftest: a wired selftest was still flagged")
            return 1
    print("ok: orphan detected, wired selftest cleared")
    return 0


def main() -> int:
    if "--selftest" in sys.argv:
        return selftest()
    orphans = check(ROOT)
    if orphans:
        for o in orphans:
            print(f"ORPHAN SELFTEST: {o} offers --selftest but no gate runs it")
        return 1
    print("ok: every --selftest acceptor is invoked by a gate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
