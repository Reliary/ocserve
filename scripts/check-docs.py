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


def gitignored_dirs() -> tuple[str, ...]:
    """Directory prefixes from .gitignore (runtime artifacts that legitimately
    do not exist in a clean checkout, e.g. bench/drift/)."""
    out: list[str] = []
    gi = os.path.join(ROOT, ".gitignore")
    if os.path.isfile(gi):
        for line in open(gi, encoding="utf-8", errors="ignore"):
            line = line.strip()
            if not line or line.startswith("#") or line.startswith("!"):
                continue
            if line.endswith("/") and "/" in line:
                out.append(line)
    return tuple(out)


IGNORED = gitignored_dirs()


# A doc path may carry a `<placeholder>` leaf (a generated artifact named by the
# build, e.g. the tag in `assets/webui/<tag>.pack.zst`). The exact token is not
# a real file, but the DIRECTORY must still exist: otherwise moving the assets
# turns the doc into a lie and the token regex (which needs a file extension)
# quietly skips it. This is the 2026-10-18 THIRD-PARTY.md finding.
PLACEHOLDER = re.compile(r'`([A-Za-z0-9_][A-Za-z0-9_./-]*)<[A-Za-z0-9_-]+>([A-Za-z0-9_./-]*)`')


def check_dead_placeholder_dirs() -> list[str]:
    bad: list[str] = []
    for name in DOCS:
        path = os.path.join(ROOT, name)
        for i, line in enumerate(open(path, encoding="utf-8", errors="ignore"), 1):
            for m in PLACEHOLDER.finditer(line):
                p = m.group(1) + "PLACEHOLDER" + m.group(2)
                if not p.startswith(PREFIXES):
                    continue
                if any(p.startswith(d) for d in IGNORED):
                    continue
                # the directory that must exist is everything before the
                # placeholder segment
                head = m.group(1)
                d = os.path.dirname(head.rstrip("/"))
                while d and not os.path.isdir(os.path.join(ROOT, d)):
                    nd = os.path.dirname(d)
                    if nd == d:
                        break
                    d = nd
                # require the deepest existing prefix to be the dir we expect,
                # i.e. the placeholder's own directory must exist
                want = head.rstrip("/")
                if want and not os.path.isdir(os.path.join(ROOT, want)):
                    bad.append(
                        f"{name}:{i}: placeholder path `{p}` names directory "
                        f"`{want}` that does not exist"
                    )
    return bad


def check_dead_paths() -> list[str]:
    bad: list[str] = []
    for name in DOCS:
        path = os.path.join(ROOT, name)
        for i, line in enumerate(open(path, encoding="utf-8", errors="ignore"), 1):
            for m in TOKEN.finditer(line):
                p = m.group(1)
                if not p.startswith(PREFIXES):
                    continue
                if any(p.startswith(d) for d in IGNORED):
                    continue  # gitignored runtime artifact (bench/drift/, …)
                if not os.path.exists(os.path.join(ROOT, p)):
                    bad.append(f"{name}:{i}: dead repo path `{p}`")
    bad.extend(check_dead_placeholder_dirs())
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


def check_closed_divergence_reopened() -> list[str]:
    """A `D-*` token DECLARED closed must not be re-listed as a CURRENT
    divergence anywhere else.

    The 2026-10-18 finding: `V2-PARITY-PLAN.md` still described
    `D-PLUGIN-INVENTORY` as "plugin list honestly empty" months after the
    divergence was closed in code. The pre-existing check only cross-read
    SECURITY.md and only against "removed" tokens, so the contradiction passed.

    A CLOSURE is `D-X: CLOSED` or `D-X … CLOSED` where CLOSED attaches to
    X — NOT the word "closed" anywhere on the line. Two shapes that produced
    false positives when this was coarse (do not re-introduce):
      * `... (closes D-TOOLS-TASK)` — English verb describing the work, the
        token is the divergence the row is ABOUT, not a closure marker;
      * `- D-RPC: CLOSED ... (see D-WEBSEARCH)` — D-RPC is closed, D-WEBSEARCH
        is a separate token that happens to share the line.
    So: the marker must follow its own token, and only the FIRST token on a
    declaring line counts.
    """
    closed: dict[str, str] = {}
    decl = re.compile(r"\b(D-[A-Z0-9-]+)\b[^|]{0,80}?\bCLOSED\b")
    for name in DOCS:
        path = os.path.join(ROOT, name)
        if not os.path.isfile(path):
            continue
        for lineno, line in enumerate(
            open(path, encoding="utf-8", errors="ignore"), start=1
        ):
            m = decl.search(line)
            if m:
                closed.setdefault(m.group(1), f"{name}:{lineno}")

    if not closed:
        return []

    # Whole-token membership: `D-WEBSEARCH` must NOT match `D-WEBSEARCH-PARTIAL`
    # (the base divergence closed; the -PARTIAL residual is current — the 2026-10-18
    # false positive). A token appears in a line only as its own bracketed word.
    def mentions(line: str, tok: str) -> bool:
        return re.search(r"(?<![A-Z0-9-])" + re.escape(tok) + r"(?![A-Z0-9-])", line) is not None

    bad: list[str] = []
    for name in DOCS:
        path = os.path.join(ROOT, name)
        if not os.path.isfile(path):
            continue
        for lineno, line in enumerate(
            open(path, encoding="utf-8", errors="ignore"), start=1
        ):
            for tok, where in closed.items():
                if f"{name}:{lineno}" == where:
                    continue
                if not mentions(line, tok):
                    continue
                low = line.lower()
                if "closed" in low or "removed" in low or "superseded" in low:
                    continue
                bad.append(
                    f"{name}:{lineno}: lists {tok} as current, but it is declared "
                    f"CLOSED at {where} (rule 31)"
                )
                break
    return bad


def run_all() -> list[str]:
    return (
        check_dead_paths()
        + check_rule_refs()
        + check_removed_divergence()
        + check_closed_divergence_reopened()
    )


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

    # plant a contradiction: declare a divergence CLOSED, then re-list it
    # elsewhere as current (the 2026-10-18 D-PLUGIN-INVENTORY shape).
    decl_victim = os.path.join(ROOT, "PLAN.md")
    list_victim = os.path.join(ROOT, "README.md")
    b1, b2 = decl_victim + ".lintbak", list_victim + ".lintbak"
    shutil.copy2(decl_victim, b1)
    shutil.copy2(list_victim, b2)
    caught_closed = False
    try:
        divergence = "D-RULE31-SELFTEST"
        with open(decl_victim, "a", encoding="utf-8") as f:
            f.write(f"\n- {divergence}: CLOSED — selftest fixture.\n")
        with open(list_victim, "a", encoding="utf-8") as f:
            f.write(f"\n(selftest) {divergence} is a current known limitation.\n")
        caught_closed = any(divergence in x for x in run_all())
    finally:
        shutil.move(b1, decl_victim)
        shutil.move(b2, list_victim)

    # plant a placeholder whose directory does not exist (the THIRD-PARTY shape)
    ph_victim = os.path.join(ROOT, DOCS[0])
    ph_backup = ph_victim + ".lintbak"
    shutil.copy2(ph_victim, ph_backup)
    caught_placeholder = False
    try:
        with open(ph_victim, "a", encoding="utf-8") as f:
            f.write("\n(selftest) `bench/__rule31_missing_dir/<tag>.pack.zst`\n")
        caught_placeholder = any("__rule31_missing_dir" in x for x in run_all())
    finally:
        shutil.move(ph_backup, ph_victim)

    if caught_path and caught_rule and caught_closed and caught_placeholder:
        print(
            "selftest: PASS (dead path + bogus rule ref + reopened closed "
            "divergence + dead placeholder dir all detected)"
        )
        return 0
    print(
        f"selftest: FAIL (path={caught_path} rule={caught_rule} closed={caught_closed} "
        f"placeholder={caught_placeholder}) — guard is vacuous",
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
    print("ok (no dead doc paths, no dangling guard-rule refs, no stale/reopened divergences)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
