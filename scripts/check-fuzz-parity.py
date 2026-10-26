#!/usr/bin/env python3
"""Fuzz-parity guard (rule 59).

Two silent-failure modes in the fuzz workspace:

1. The fuzzer resolves `serde_json` with a different feature set than the
   shipped workspace. On 2026-10-26 the fuzzer had neither `preserve_order` nor
   `arbitrary_precision`; splicing an object carrying
   `$serde_json::private::Number` "succeeded" under the fuzzer and failed in
   production, so the fuzzer reported green on code that emitted unparseable
   JSON.
2. A target with committed regression seeds is missing from the CI matrix, so
   those seeds never run.

`fuzz/corpus/` is gitignored (cargo-fuzz's own corpus is huge), so seeds that
must survive live under `fuzz/seeds/<target>/` and are copied in by the
workflow. This check keeps both facts true.

Usage: check-fuzz-parity.py [--selftest]
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def features(text: str, crate: str) -> set[str]:
    """Feature names declared for `crate` in a manifest (workspace or plain)."""
    # Match the dependency line through its closing brace on the same or the
    # following lines; `features = [..]` is the only list we care about.
    m = re.search(rf'^{crate}\s*=\s*(\{{.*?\}}|"[^"]*")', text, re.S | re.M)
    if not m:
        return set()
    block = m.group(1)
    fm = re.search(r'features\s*=\s*\[([^\]]*)\]', block, re.S)
    return set(re.findall(r'"([a-z_]+)"', fm.group(1))) if fm else set()


def check(root: pathlib.Path = ROOT) -> list[str]:
    problems = []
    main = (root / "Cargo.toml").read_text()
    fuzz_path = root / "fuzz/Cargo.toml"
    if not fuzz_path.exists():
        return [f"missing {fuzz_path}"]
    fuzz = fuzz_path.read_text()

    # Production feature set = the workspace declaration PLUS features that
    # arrive transitively. `arbitrary_precision` is enabled by
    # `rolldown_common` (verify: cargo tree -e features -p ocserve | grep
    # arbitrary_precision), and cargo unifies features across the graph, so the
    # shipped server always parses with it on. The fuzzer must match.
    TRANSITIVE = {"arbitrary_precision"}
    main_feats = features(main, "serde_json") | TRANSITIVE
    fuzz_feats = features(fuzz, "serde_json")
    missing = main_feats - fuzz_feats
    if missing:
        problems.append(
            f"fuzz serde_json lacks workspace features {sorted(missing)}: the "
            "fuzzer would exercise a different parser than production"
        )

    wf_path = root / ".github/workflows/fuzz.yml"
    if not wf_path.exists():
        return problems + ["missing .github/workflows/fuzz.yml"]
    wf = wf_path.read_text()
    targets = re.findall(r'^name = "([a-z_]+)"$', fuzz, re.M)
    if not targets:
        problems.append("no fuzz targets parsed from fuzz/Cargo.toml")
    for t in targets:
        seeds = root / "fuzz/seeds" / t
        if seeds.is_dir() and not re.search(rf"^\s*-\s*{t}\s*$", wf, re.M):
            problems.append(f"{t} has committed seeds but is absent from the CI matrix")
    return problems


def main() -> int:
    if "--selftest" in sys.argv:
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as td:
            tmp = pathlib.Path(td)
            shutil.copytree(ROOT / "fuzz", tmp / "fuzz", ignore=shutil.ignore_patterns("target", "corpus", "artifacts"))
            shutil.copytree(ROOT / ".github", tmp / ".github")
            shutil.copy(ROOT / "Cargo.toml", tmp / "Cargo.toml")
            if check(tmp):
                print("FAIL(selftest): clean tree flagged")
                return 1
            # plant 1: drop a feature
            f = tmp / "fuzz/Cargo.toml"
            f.write_text(re.sub(r',\s*"arbitrary_precision"', "", f.read_text()))
            if not check(tmp):
                print("FAIL(selftest): feature mismatch not detected")
                return 1
            shutil.copy(ROOT / "fuzz/Cargo.toml", f)
            # plant 2: de-list a seeded target
            w = tmp / ".github/workflows/fuzz.yml"
            w.write_text(w.read_text().replace("          - splice_parser\n", ""))
            if not check(tmp):
                print("FAIL(selftest): de-listed seeded target not detected")
                return 1
        print("selftest: PASS (feature mismatch and de-listed seeded target detected)")
        return 0

    problems = check()
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        return 1
    fuzz = (ROOT / "fuzz/Cargo.toml").read_text()
    n = len(re.findall(r'^name = "([a-z_]+)"$', fuzz, re.M))
    print(f"ok ({n} fuzz targets; serde_json features match the workspace)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
