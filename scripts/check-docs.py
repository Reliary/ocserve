#!/usr/bin/env python3
"""Doc-lint guard (rule 31).

Docs are the spec (AGENTS §1), but nothing checked them against the repo, so
dead references accumulated and only surfaced by hand (the 2026-10-13 audit:
`bench/sdk-routes.txt`, `bench/webui-routes.txt`, `scripts/pgo-build.sh` all
referenced but gone; two `K-IMPORT` rows; a `D-VCS-NOAPPLY` divergence still
listed after it was removed).

This checks the top-level spec docs for mechanically-detectable staleness:

  1. A backticked repo-relative path (`bench/…`, `scripts/…`, `crates/…`,
     `deploy/…`, `.github/…`, `testdata/…`, `.cargo/…`) with a file extension
     must exist on disk. Directory-only refs (no extension) are skipped.
  2. A `rule N` / `rules N` reference in prose must name a rule that actually
     exists (a `rule N` marker in scripts/check-guards.sh, or a registered
     external-rule number). Retired-rule mentions must say "retired".
  3. The `D-<NAME>` divergence tokens cited in docs must not name a divergence
     that a doc explicitly marks as removed elsewhere (cross-doc). We only
     check the obvious form: a token that appears in a "removed" sentence
     somewhere AND is asserted as "current"/"known" elsewhere is flagged only
     when both occur in SECURITY.md (the "current known divergences" list).

Usage: scripts/check-docs.py [--selftest]
Exit 0 + summary on success; exit 1 on any finding.
--selftest plants a dead path / a bogus rule ref and requires detection.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOCS = sorted(
    p.name
    for p in (__import__("pathlib").Path(ROOT).glob("*.md"))
)
PREFIXES = ("bench/", "scripts/", "crates/", "deploy/", ".github/", "testdata/", ".cargo/")
# file-like tokens: something/with/a/dot.ext
TOKEN = re.compile(r'`([A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z0-9]+)`')
RULE_REF = re.compile(r'\brule s?(\d+)\b')

# External rules that live in another script (number -> where). Rule numbers
# implemented as markers in check-guards.sh are discovered dynamically.
EXTERNAL_RULES: dict[int, str] = {}  # e.g. {26: "soak-gate.sh"} — keep empty if all inline


def existing_guard_rules() -> set[int]:
    rules: set[int] = set()
    guards = os.path.join(ROOT, "scripts", "check-guards.sh")
    if os.path.isfile(guards):
        text = open(guards, encoding="utf-8", errors="ignore").read()
        rules |= {int(m.group(1)) for m in re.finditer(r'\brule (\d+)\b', text)}
        # header comment enumerates "^#   N." lines too
        for line in text.splitlines():
            m = re.match(r'#\s+(\d+)[,.]?', line)
            if m:
                rules.add(int(m.group(1)))
    rules |= set(EXTERNAL_RULES)
    return rules


def check_dead_paths() -> list[str]:
    bad: list[str] = []
    for name in DOCS:
        path = os.path.join(ROOT, name)
        for i, line in enumerate(open(path, encoding="utf-8", errors="ignore"), 1):
            for m in TOKEN.finditer(line):
                p = m.group(1)
                if p.startswith(PREFIXES) and not os.path.exists(os.path.join(ROOT, p)):
                    bad.append(f"{name}:{i}: dead repo path `{p}`")
    return bad


def check_rule_refs() -> list[str]:
    bad: list[str] = []
    known = existing_guard_rules()
    for name in DOCS:
        path = os.path.join(ROOT, name)
        for i, line in enumerate(open(path, encoding="utf-8", errors="ignore"), 1):
            for m in RULE_REF.finditer(line):
                n = int(m.group(1))
                if n in known:
                    continue
                # a mention that explicitly says "retired" is allowed
                if "retired" in line.lower():
                    continue
                bad.append(f"{name}:{i}: references guard rule {n}, which is not implemented")
    return bad


def check_removed_divergence() -> list[str]:
    """SECURITY.md's current-divergence list must not name a token that another
    doc marks as removed."""
    sec = os.path.join(ROOT, "SECURITY.md")
    if not os.path.isfile(sec):
        return []
    removed: set[str] = set()
    for name in DOCS:
        for line in open(os.path.join(ROOT, name), encoding="utf-8", errors="ignore"):
            if "removed" in line.lower():
                removed |= set(re.findall(r"D-[A-Z0-9-]+", line))
    bad: list[str] = []
    text = open(sec, encoding="utf-8", errors="ignore").read()
    # only the "Current known divergences" section
    idx = text.find("Current known divergences")
    section = text[idx:] if idx >= 0 else text
    for tok in set(re.findall(r"D-[A-Z0-9-]+", section)):
        if tok in removed:
            bad.append(f"SECURITY.md: lists {tok} as a current divergence but it is marked removed")
    return bad


def run_all() -> list[str]:
    return check_dead_paths() + check_rule_refs() + check_removed_divergence()


def selftest() -> int:
    import tempfile
    import shutil

    # plant a dead path ref and a bogus rule ref in a real doc, then restore
    victim = os.path.join(ROOT, DOCS[0])
    backup = victim + ".lintbak"
    shutil.copy2(victim, backup)
    try:
        with open(victim, "a", encoding="utf-8") as f:
            f.write("\n(selftest) `bench/__rule31_missing.txt` and rule 991 mention.\n")
        findings = run_all()
        caught_path = any("__rule31_missing.txt" in x for x in findings)
        caught_rule = any("rule 991" in x for x in findings)
    finally:
        shutil.move(backup, victim)
    if caught_path and caught_rule:
        print("selftest: PASS (dead path + bogus rule ref both detected)")
        return 0
    print(
        f"selftest: FAIL (path={caught_path} rule={caught_rule}) — guard is vacuous",
        file=sys.stderr,
    )
    return 1


def main() -> int:
    if "--selftest" in sys.argv:
        return selftest()
    findings = run_all()
    if findings:
        for x in findings:
            print(f"FAIL: {x}")
        print(f"\n{len(findings)} doc staleness finding(s) — fix or correct the doc (rule 31).")
        return 1
    print("ok (no dead doc paths, no dangling guard-rule refs, no stale divergences)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
