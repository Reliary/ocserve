#!/usr/bin/env python3
"""Rule 71 — every `module::test` citation in TRACEABILITY.md must exist.

The 2026-11-14 review of PR #43 found five cited test identifiers that exist
nowhere in the tree, and a matrix row that contradicted the PR's own body. The
existing check in `check-matrix.sh` only looked at eight module prefixes (22 of
76 citations) and matched the *last* segment as a substring, so a renamed test
or a citation invented from memory passed silently.

This check is derived, not a list:
  * a citation is `ident[::ident]` in backticks inside TRACEABILITY.md,
  * a citation resolves when its last segment is a real test name (from
    `cargo test -- --list`), a real source function (`fn name`), a real
    source field, or a real module path (`mod name` / `pub mod name`),
  * a `*::tests`/`*_tests` group resolves when at least one listed test lives
    under it,
  * otherwise it must be on the small allowlist below, with a reason.

Anything else is a claim that outran the code. Exit 1 on a violation.

Usage: check-matrix-citations.py <test-list-file> [--selftest]
`--selftest` plants a fabricated citation in a temp copy and requires a
violation, so a future change that silently stops matching fails here.
"""
import glob
import os
import re
import sys
import tempfile

TRACE = "TRACEABILITY.md"

# Citations that name something other than a test or a source symbol. Each
# entry names why. Keep this list short: an entry here is a reviewed claim.
ALLOW = {
    "libc::c_char": "C type from the libc crate (PSA/platform notes)",
    "std::os::unix": "std module path (platform notes)",
}

CITE_RE = re.compile(
    r"`([A-Za-z_][A-Za-z0-9_]*::[a-z_][a-z0-9_]*(?:::[a-z_][a-z0-9_]*)?)`"
)


def load_tests(path):
    with open(path, encoding="utf-8") as f:
        return [l.strip() for l in f if l.strip()]


def load_symbols():
    fns, mods, fields = set(), set(), set()
    for p in glob.glob("crates/*/src/**/*.rs", recursive=True):
        try:
            s = open(p, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        fns.update(re.findall(r"\bfn\s+([a-z_][a-z0-9_]*)", s))
        mods.update(re.findall(r"\bmod\s+([a-z_][a-z0-9_]*)", s))
        # Public field declarations (`pub attribution_session: Option<String>`)
        # are legitimate matrix citations for option/struct names.
        fields.update(re.findall(r"\bpub\s+([a-z_][a-z0-9_]*)\s*:", s))
    return fns, mods, fields


def violations(trace_text, tests, fns, mods, fields):
    last = {t.split("::")[-1] for t in tests}
    out = []
    for cite in sorted(set(CITE_RE.findall(trace_text))):
        seg = cite.split("::")
        tail = seg[-1]
        if cite in ALLOW:
            continue
        if tail in last or tail in fns or tail in mods or tail in fields:
            continue
        if (tail == "tests" or tail.endswith("_tests")) and any(
            t.startswith(cite + "::") for t in tests
        ):
            continue
        # A one-segment-prefixed group whose members are `mod::tests::…`
        if any(t.startswith(cite + "::") for t in tests):
            continue
        out.append(cite)
    return out


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: check-matrix-citations.py <test-list-file> [--selftest]",
              file=sys.stderr)
        return 2
    if not os.path.exists(TRACE):
        print(f"FAIL: {TRACE} not found (run from the repo root)", file=sys.stderr)
        return 1

    tests = load_tests(sys.argv[1])
    if not tests:
        # An empty list means the harness failed to enumerate, not that every
        # citation is an orphan; refusing to pass is the only honest answer.
        print("FAIL: test list is empty (harness/environment)", file=sys.stderr)
        return 1

    fns, mods, fields = load_symbols()
    text = open(TRACE, encoding="utf-8").read()

    if "--selftest" in sys.argv:
        plant = "some_module::a_test_that_was_invented_from_memory"
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write(text + f"\n| `{plant}` | planted |\n")
            planted = f.name
        try:
            got = violations(open(planted, encoding="utf-8").read(), tests, fns, mods, fields)
        finally:
            os.unlink(planted)
        if plant not in got:
            print("FAIL: planted fabricated citation was not detected (vacuous)",
                  file=sys.stderr)
            return 1
        print("ok (planted fabricated citation detected)")
        return 0

    bad = violations(text, tests, fns, mods, fields)
    if bad:
        for c in bad:
            print(f"FAIL: matrix cites `{c}` but no test, function or module has that name")
        return 1
    print(f"ok ({len(set(CITE_RE.findall(text)))} citations resolve to a test, function or module)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
