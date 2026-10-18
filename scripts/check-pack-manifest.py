#!/usr/bin/env python3
"""Embedded-UI pack/manifest consistency (guard rule 17, part 2).

Each pinned web UI has a pack (`<tag>.pack.zst`) and a manifest
(`<tag>.manifest.json`). They MUST describe the same file set: the extractor
writes both atomically, but a re-run that regenerated only one left a pack with
173 entries next to a 1678-entry manifest (2026-10-18) — the binary then served
an incomplete bundle, and only the Playwright browser check caught it. This
makes the mismatch a commit-time failure.

Also asserts the pack is a known format (OCAP1/OCAP2) so a truncation/corruption
is caught here rather than at boot.

Selftest: a deliberately mismatched pair must FAIL (proves non-vacuous).
Usage: check-pack-manifest.py   (no args; scans crates/ocserve-http/assets/webui)
"""
import json
import pathlib
import struct
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIR = ROOT / "crates/ocserve-http/assets/webui"


def pack_count(path: pathlib.Path) -> tuple[int, str]:
    raw = path.read_bytes()
    magic = raw[:6]
    if magic == b"OCAP2\n":
        (n,) = struct.unpack_from("<I", raw, 6)
        return n, "OCAP2"
    if magic == b"OCAP1\n":
        (n,) = struct.unpack_from("<I", raw, 6)
        return n, "OCAP1"
    return -1, f"unknown({magic!r})"


def check(dir_: pathlib.Path) -> list[str]:
    errs = []
    packs = sorted(dir_.glob("*.pack.zst"))
    if not packs:
        return [f"no packs under {dir_}"]
    for pack in packs:
        tag = pack.name[: -len(".pack.zst")]
        man = dir_ / f"{tag}.manifest.json"
        if not man.exists():
            errs.append(f"{tag}: pack without manifest")
            continue
        n, fmt = pack_count(pack)
        if n < 0:
            errs.append(f"{tag}: bad pack format {fmt}")
            continue
        m = len(json.loads(man.read_text())["files"])
        if n != m:
            errs.append(f"{tag}: pack has {n} entries, manifest has {m} (mismatch)")
    return errs


def selftest() -> bool:
    import tempfile
    with tempfile.TemporaryDirectory() as td:
        d = pathlib.Path(td)
        pack = d / "t.pack.zst"
        pack.write_bytes(b"OCAP2\n" + struct.pack("<I", 3))
        (d / "t.manifest.json").write_text(json.dumps({"files": {"a": 1, "b": 2}}))
        if not check(d):
            print("selftest: mismatched pack/manifest NOT flagged (vacuous)", file=sys.stderr)
            return False
    return True


def main() -> int:
    if not selftest():
        return 3
    errs = check(DIR)
    if errs:
        for e in errs:
            print(f"FAIL: {e}", file=sys.stderr)
        return 1
    packs = list(DIR.glob("*.pack.zst"))
    print(f"ok: {len(packs)} pack/manifest pairs consistent (entry counts + format)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
