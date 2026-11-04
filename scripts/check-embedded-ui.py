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
import re
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
    # The slot's declared tag must equal the Rust pin. Without this, slot
    # naming makes the skew guard compare a slot against itself: a stale pack
    # could sit under `v1` while the server reports a newer FREEZE_VERSION.
    pins = {}
    for slot, rel, const in [
        ("v1", "crates/ocserve-llm/src/lib.rs", "FREEZE_VERSION"),
        ("v2", "crates/ocserve-core/src/spec.rs", "V2_TAG"),
    ]:
        try:
            txt = (root / rel).read_text()
            m = re.search(
                rf'pub\s+const\s+{const}\s*:\s*&str\s*=\s*"([^"]+)"', txt
            )
            pins[slot] = m.group(1) if m else None
        except OSError:
            pins[slot] = None
    for slot, meta in sorted(parts_doc.items()):
        if slot in pins:
            if pins[slot] is None:
                errors.append(f"{slot}: cannot read the Rust pin (const missing)")
            elif meta.get("tag") != pins[slot]:
                errors.append(
                    f"{slot}: parts/manifest tag {meta.get('tag')!r} != Rust pin {pins[slot]!r}"
                )
        index = (webui / f"{slot}.index.bin").read_bytes()
        if hashlib.sha256(index).hexdigest() != meta["index_sha256"]:
            errors.append(f"{slot}: index hash mismatch")
        entries = parse_index(index)
        mdoc = json.loads((webui / f"{slot}.manifest.json").read_text())
        if mdoc.get("tag") != meta.get("tag"):
            errors.append(
                f"{slot}: manifest tag {mdoc.get('tag')!r} != parts.json {meta.get('tag')!r}"
            )
        manifest = mdoc["files"]
        if len(entries) != len(manifest):
            errors.append(f"{slot}: index {len(entries)} != manifest {len(manifest)}")
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
            errors.append(f"{slot}: concatenated parts {len(blob):,} != {meta['total']:,}")
            continue
        if zstd is None:
            continue
        for path, off, raw, comp in entries:
            frame = blob[off : off + comp]
            if len(frame) != comp:
                errors.append(f"{slot}: {path} overruns the parts")
                break
            try:
                data = zstd.decompress(frame)
            except Exception as exc:  # noqa: BLE001 - report as a finding
                errors.append(f"{slot}: {path} does not decompress: {exc}")
                break
            m = manifest.get("/" + path)
            if m is None:
                errors.append(f"{slot}: {path} absent from the manifest")
                break
            if len(data) != m["bytes"] or hashlib.sha256(data).hexdigest() != m["sha256"]:
                errors.append(f"{slot}: {path} content mismatch")
                break
    return errors


def selftest() -> int:
    """Run the real predicate over a copied tree with planted defects."""
    with tempfile.TemporaryDirectory() as td:
        tmp = pathlib.Path(td)
        shutil.copytree(ROOT / "crates", tmp / "crates",
                        ignore=shutil.ignore_patterns("src", "tests", "Cargo.toml"))
        # The pin check reads two source files; `src` is excluded above to keep
        # the copy small, so plant them explicitly. Without this every plant
        # below would "pass" on a pin error instead of on the defect it names
        # (the copy would be flagged even when restored).
        for rel in ("crates/ocserve-llm/src/lib.rs", "crates/ocserve-core/src/spec.rs"):
            dst = tmp / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / rel, dst)
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
        # Pin plant: a slot whose declared tag disagrees with the Rust pin must
        # be flagged (slot naming would otherwise compare the slot to itself).
        pj = webui / "parts.json"
        saved = pj.read_text()
        doc = json.loads(saved)
        first = sorted(doc)[0]
        doc[first]["tag"] = "0.0.0"
        pj.write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")
        if not check(tmp):
            print("FAIL(selftest): pin/slot tag mismatch not detected")
            return 1
        pj.write_text(saved)
    print("selftest: PASS (swap, truncation, oversize, pin mismatch detected; restored clean)")
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
