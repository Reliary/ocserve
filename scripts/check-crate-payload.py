#!/usr/bin/env python3
"""Crate-payload size guard (rule 38).

crates.io rejects any published `.crate` above a size limit (10 MiB by
default; raised per-crate on request through a `max_upload_size` row). The
existing `cargo package --list` gate validates PACKAGE LAYOUT only — it never
looks at bytes — so a crate can silently grow past the limit and the failure
only appears at publish time, when the whole ordered publish chain aborts at
that crate (2026-10-18 finding: `ocserve-http` carries two embedded web-UI
packs, 9.7 MB + 24.4 MB, so its packed payload is ~35 MB).

This derives each member's packed payload size from `cargo package --list`
(the exact file set that would ship) plus on-disk sizes, and fails when any
member exceeds the cap declared in publish.yml.

The cap is read from `.github/workflows/publish.yml`
(`env.CRATE_PAYLOAD_LIMIT_BYTES`, default 10 MiB) so the registry's real limit
has exactly one declaration.

Exit 0 + a size table on success; exit 1 on any over-cap crate.
--selftest inflates a fixture member above the cap and requires detection
(proves the check is load-bearing, TESTING §1).
"""
import os
import re
import subprocess
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PUBLISH_YML = ROOT / ".github/workflows/publish.yml"
DEFAULT_LIMIT = 10 * 1024 * 1024


def crate_dirs() -> list:
    """[(name, path)] for every workspace member, in dependency-free order."""
    manifest = (ROOT / "Cargo.toml").read_text()
    m = re.search(r"members\s*=\s*\[(.*?)\]", manifest, re.S)
    if not m:
        raise SystemExit("ERR: no [workspace] members in Cargo.toml")
    out = []
    for rel in re.findall(r'"([^"]+)"', m.group(1)):
        path = ROOT / rel
        ct = (path / "Cargo.toml").read_text()
        name = re.search(r'^name\s*=\s*"([^"]+)"', ct, re.M)
        if not name:
            raise SystemExit(f"ERR: no package name in {rel}/Cargo.toml")
        out.append((name.group(1), path))
    return out


def limit_bytes(yml_text: str) -> int:
    m = re.search(r"CRATE_PAYLOAD_LIMIT_BYTES:\s*(\d+)", yml_text)
    return int(m.group(1)) if m else DEFAULT_LIMIT


def packed_files(crate: str) -> list:
    """Paths (relative to the crate root) cargo would ship."""
    r = subprocess.run(
        ["cargo", "package", "-p", crate, "--list", "--allow-dirty"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise SystemExit(f"ERR: cargo package --list failed for {crate}:\n{r.stderr.strip()}")
    return [ln.strip() for ln in r.stdout.splitlines() if ln.strip()]


def payload_size(crate_dir: pathlib.Path, files: list) -> int:
    total = 0
    for rel in files:
        p = crate_dir / rel
        if p.is_file():
            total += p.stat().st_size
    return total


def check(limit: int, table: dict, extra: int = 0) -> list:
    """Problems for crates over the cap. `extra` inflates the named crate
    (selftest only) so the predicate is provably load-bearing."""
    problems = []
    for name, size in sorted(table.items(), key=lambda kv: -kv[1]):
        eff = size + (extra if name == max(table, key=table.get) else 0)
        if eff > limit:
            problems.append(
                f"{name}: packed payload {eff:,} B exceeds the "
                f"{limit:,} B cap (CRATE_PAYLOAD_LIMIT_BYTES)"
            )
    return problems


def main() -> int:
    yml = PUBLISH_YML.read_text()
    limit = limit_bytes(yml)

    if "--selftest" in sys.argv:
        # Plant a violation: pretend the largest crate carries `limit` extra
        # bytes and require the same predicate to flag it.
        table = {"ocserve-selftest-fixture": limit + 1}
        if not check(limit, table):
            print("FAIL(selftest): an over-cap crate was not detected")
            return 1
        # And a control: a table under the cap must pass.
        if check(limit, {"ocserve-selftest-fixture": limit - 1}):
            print("FAIL(selftest): an under-cap crate was flagged")
            return 1
        print("selftest: PASS (over-cap detected, under-cap clean)")
        return 0

    table = {}
    for name, path in crate_dirs():
        table[name] = payload_size(path, packed_files(name))

    problems = check(limit, table)
    if problems:
        for p in problems:
            print(f"FAIL: {p} (rule 38)")
        return 1

    biggest = max(table.values())
    print(
        f"ok (all {len(table)} members under the {limit:,} B cap; "
        f"largest {biggest:,} B = {100 * biggest / limit:.1f}% of cap)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())