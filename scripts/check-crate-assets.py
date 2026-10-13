#!/usr/bin/env python3
"""Crate-asset guard (rule 30).

A compiled-in asset (`include_str!` / `include_bytes!`) whose path escapes the
owning crate's root is NOT packaged into the `.crate` tarball. `cargo build`
and `cargo test` still pass locally (the file exists in the monorepo), and
`cargo publish --no-verify` uploads a broken crate — so the failure only
surfaces when an end user runs `cargo install`, which is exactly the 0.3.0
defect: `ocserve-tools`/`ocserve-http`/`ocserve-core` embedded files from
`bench/`, outside every crate root, and `cargo install ocserve` failed to
compile with "couldn't read .../bench/...".

This guard resolves every literal `include_*!` path against its source file and
fails if it lands outside the crate root. It is the static half; the dynamic
half is `cargo package` (which compiles the packaged crate) in
`.github/workflows/ci.yml` and `publish.yml`.

Usage:
  scripts/check-crate-assets.py [--selftest]

Exit 0 + summary on success; exit 1 on any escaping include.
`--selftest` plants an escaping include and requires the scan to flag it.
"""
import os
import re
import sys
import tempfile

INC = re.compile(r'include_(?:str|bytes)!\s*\(\s*"([^"]+)"\s*\)')
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def crate_of(src_path: str):
    """Return (crate_root, rel_under_crate) for a file under crates/<name>/..."""
    rel = os.path.relpath(src_path, ROOT)
    parts = rel.split(os.sep)
    if len(parts) < 3 or parts[0] != "crates":
        return None, None
    return os.path.join(ROOT, parts[0], parts[1]), rel


def scan() -> list[str]:
    """Return a list of 'src:line: path -> resolves outside crate root' strings."""
    bad: list[str] = []
    crates_dir = os.path.join(ROOT, "crates")
    if not os.path.isdir(crates_dir):
        return bad
    for crate in sorted(os.listdir(crates_dir)):
        crate_root = os.path.join(crates_dir, crate)
        if not os.path.isdir(os.path.join(crate_root, "src")):
            continue
        for dirpath, _dirs, files in os.walk(crate_root):
            for name in files:
                if not name.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, name)
                try:
                    text = open(full, encoding="utf-8", errors="ignore").read()
                except OSError:
                    continue
                for m in INC.finditer(text):
                    ref = m.group(1)
                    resolved = os.path.normpath(os.path.join(dirpath, ref))
                    if not (resolved == crate_root or resolved.startswith(crate_root + os.sep)):
                        line = text[: m.start()].count("\n") + 1
                        bad.append(
                            f"{os.path.relpath(full, ROOT)}:{line}: "
                            f"{ref!r} resolves outside crate root ({crate}/)"
                        )
    return bad


def selftest() -> int:
    """Plant an escaping include and require it to be flagged."""
    crates = os.path.join(ROOT, "crates")
    target_crate = None
    for c in sorted(os.listdir(crates)):
        if os.path.isdir(os.path.join(crates, c, "src")):
            target_crate = c
            break
    if target_crate is None:
        print("selftest: no crate with src/ to plant into", file=sys.stderr)
        return 1
    plant = os.path.join(crates, target_crate, "src", "__rule30_plant.rs")
    with open(plant, "w") as f:
        f.write('const X: &str = include_str!("../../../nonexistent-escape.txt");\n')
    try:
        bad = scan()
        caught = any("__rule30_plant.rs" in b for b in bad)
    finally:
        os.remove(plant)
    if caught:
        print("selftest: PASS (planted crate-escaping include detected)")
        return 0
    print("selftest: FAIL (escaping include not detected — guard is vacuous)", file=sys.stderr)
    return 1


def main() -> int:
    if "--selftest" in sys.argv:
        return selftest()
    bad = scan()
    if bad:
        for b in bad:
            print(f"FAIL: {b}")
        print(
            f"\n{len(bad)} compiled-in asset(s) escape their crate root — move each "
            "into the owning crate's assets/ so it ships in the .crate tarball (rule 30)."
        )
        return 1
    print("ok (no compiled-in asset escapes its crate root)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
