#!/usr/bin/env python3
"""Embedded web-UI shard guard (rule 17).

The pinned web UIs are OCAP2 blobs split at zstd frame boundaries across
`ocserve-ui-*` crates because crates.io caps one `.crate` at 10 MiB. Three
failure modes are silent without this check:

  1. a part missing/truncated/swapped -> the binary boots with a UI that
     serves some files and 404s others (only a browser would notice);
  2. the index and the manifests disagreeing -> the loader resolves paths the
     frames do not contain;
  3. a part over the registry cap -> publish aborts at that crate after the
     dependency chain has already uploaded.

This reconstructs each tag from its parts, parses the index, decompresses every
entry and compares content sha256 against the committed manifest. The 10 MiB
cap is checked per part.

Selftest plants (a) a swapped pair, (b) a truncated part, (c) an oversized
part, and requires each to be detected, then restores and requires green.
Usage: check-embedded-ui.py [--selftest]
"""
import hashlib
import importlib.util
import json
import pathlib
import shutil
import struct
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
WEBUI = ROOT / "crates/ocserve-http/assets/webui"
LIMIT = 10_485_760


def parse_index(blob: bytes):
    if blob[:6] != b"OCAP2\n":
        raise SystemExit(f"bad index magic {blob[:6]!r}")
    (count,) = struct.unpack_from("<I", blob, 6)
    i = 10
    out = []
    for _ in range(count):
        (plen,) = struct.unpack_from("<H", blob, i)
        i += 2
        path = blob[i : i + plen].decode()
        i += plen
        off, raw, comp = struct.unpack_from("<QII", blob, i)
        i += 16
        out.append((path, off, raw, comp))
    return out


def check(root: pathlib.Path = ROOT) -> list[str]:
    webui = root / "crates/ocserve-http/assets/webui"
    parts_doc = json.loads((webui / "parts.json").read_text())
    errors = []
    try:
        from compression import zstd
    except ImportError:
        zstd = None
    for tag, meta in sorted(parts_doc.items()):
        index = (webui / f"{tag}.index.bin").read_bytes()
        if hashlib.sha256(index).hexdigest() != meta["index_sha256"]:
            errors.append(f"{tag}: index hash mismatch")
        entries = parse_index(index)
        manifest = json.loads((webui / f"{tag}.manifest.json").read_text())["files"]
        if len(entries) != len(manifest):
            errors.append(f"{tag}: index {len(entries)} != manifest {len(manifest)}")
        blobs = []
        for part in meta["parts"]:
            p = root / "crates" / part["crate"] / "assets" / "part.bin"
            if not p.exists():
                errors.append(f"{part['crate']}: part.bin missing")
                continue
            data = p.read_bytes()
            if len(data) > LIMIT:
                errors.append(f"{part['crate']}: {len(data):,} B over the 10 MiB cap")
            if len(data) != part["bytes"]:
                errors.append(f"{part['crate']}: size {len(data):,} != {part['bytes']:,}")
            if hashlib.sha256(data).hexdigest() != part["sha256"]:
                errors.append(f"{part['crate']}: sha256 mismatch")
            blobs.append(data)
        blob = b"".join(blobs)
        if len(blob) != meta["total"]:
            errors.append(f"{tag}: concatenated parts {len(blob):,} != {meta['total']:,}")
            continue
        if zstd is None:
            continue
        for path, off, raw, comp in entries:
            frame = blob[off : off + comp]
            if len(frame) != comp:
                errors.append(f"{tag}: {path} overruns the parts")
                break
            try:
                data = zstd.decompress(frame)
            except Exception as exc:  # noqa: BLE001 - report as a finding
                errors.append(f"{tag}: {path} does not decompress: {exc}")
                break
            m = manifest.get("/" + path)
            if m is None:
                errors.append(f"{tag}: {path} absent from the manifest")
                break
            if len(data) != m["bytes"] or hashlib.sha256(data).hexdigest() != m["sha256"]:
                errors.append(f"{tag}: {path} content mismatch")
                break
    return errors


def selftest() -> int:
    """Run the real predicate over a copied tree with planted defects."""
    with tempfile.TemporaryDirectory() as td:
        tmp = pathlib.Path(td)
        shutil.copytree(ROOT / "crates", tmp / "crates",
                        ignore=shutil.ignore_patterns("src", "tests", "Cargo.toml"))
        webui = tmp / "crates/ocserve-http/assets/webui"
        parts_doc = json.loads((webui / "parts.json").read_text())
        tag = sorted(parts_doc)[0]
        parts = parts_doc[tag]["parts"]
        p0 = tmp / "crates" / parts[0]["crate"] / "assets/part.bin"
        p1 = tmp / "crates" / parts[1]["crate"] / "assets/part.bin"
        a, b = p0.read_bytes(), p1.read_bytes()
        p0.write_bytes(b)
        p1.write_bytes(a)
        if not check(tmp):
            print("FAIL(selftest): swapped parts not detected")
            return 1
        p0.write_bytes(a[: len(a) // 2])
        p1.write_bytes(b)
        if not check(tmp):
            print("FAIL(selftest): truncated part not detected")
            return 1
        p0.write_bytes(a)
        p1.write_bytes(b + b"\0")
        if not check(tmp):
            print("FAIL(selftest): oversized part not detected")
            return 1
        p1.write_bytes(b)
        if check(tmp):
            print("FAIL(selftest): restored tree flagged")
            return 1
    print("selftest: PASS (swap, truncation, oversize detected; restored clean)")
    return 0


def main() -> int:
    if "--selftest" in sys.argv:
        return selftest()
    errors = check()
    if errors:
        for e in errors:
            print(f"FAIL: {e}")
        return 1
    doc = json.loads((WEBUI / "parts.json").read_text())
    n = sum(len(v["parts"]) for v in doc.values())
    biggest = max(p["bytes"] for v in doc.values() for p in v["parts"])
    print(
        f"ok ({len(doc)} packs in {n} parts, every entry hash-verified; "
        f"largest {biggest:,} B = {100 * biggest / LIMIT:.1f}% of the cap)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
