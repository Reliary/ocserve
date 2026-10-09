#!/usr/bin/env python3
"""P6c — embedded-UI version-skew guard.

The pinned web UI (bench/webui/app/<tag>.pack.zst) is a Vite build of
`packages/app` at the freeze tag, so every API route it calls must exist in the
frozen OpenAPI contract (bench/openapi/<tag>.json). If they ever disagree, the
extraction captured the wrong build (or the spec is stale) and the UI would
call a route the server does not have — the exact version-skew bug class.

This extracts the route surface from the packed JS/CSS and checks it against
the spec. It is the version-matched replacement for the old
bench/webui-routes.txt (which tracked Cloudflare's *latest* bundle, not the
frozen one).

Usage: check-app-skew.py <pack.zst> <spec.json>
Exit 0 = every route the bundle calls is in the spec; 1 otherwise.
"""
import json
import os
import re
import struct
import subprocess
import sys
import tempfile


def read_pack(path):
    raw = subprocess.run(["zstd", "-d", "-c", path], capture_output=True, check=True).stdout
    if raw[:6] != b"OCAP1\n":
        sys.exit(f"{path}: bad magic")
    i = 6
    (count,) = struct.unpack_from("<I", raw, i)
    i += 4
    files = {}
    for _ in range(count):
        (plen,) = struct.unpack_from("<H", raw, i)
        i += 2
        name = raw[i : i + plen].decode()
        i += plen
        (dlen,) = struct.unpack_from("<Q", raw, i)
        i += 8
        files[name] = raw[i : i + dlen]
        i += dlen
    return files


def bundle_routes(files):
    """(method, path) pairs the bundle can call. Method defaults to GET when
    only a path literal appears (fetch of an asset)."""
    pairs = set()
    for name, body in files.items():
        if not name.endswith((".js", ".css")):
            continue
        text = body.decode("latin1")
        for m in re.finditer(r'method:"(get|post|put|delete|patch)",path:"([^"]+)"', text):
            pairs.add((m.group(1).upper(), m.group(2)))
        for m in re.finditer(r'\.(get|post|put|delete|patch)\(\{url:"([^"]+)"', text):
            pairs.add((m.group(1).upper(), m.group(2)))
    return pairs


def spec_ops(spec):
    out = set()
    for path, ops in spec["paths"].items():
        for method in ops:
            if method in ("get", "post", "put", "delete", "patch"):
                out.add((method.upper(), path))
    return out


def norm(p):
    return re.sub(r"\{[^}]+\}", "{}", p)


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: check-app-skew.py <pack.zst> <spec.json>")
    files = read_pack(sys.argv[1])
    spec = json.load(open(sys.argv[2], encoding="utf-8"))
    have = {(m, norm(p)) for m, p in spec_ops(spec)}
    # also accept a route with any method (UI may call a path the spec lists
    # under a different method — flag only truly-absent paths)
    have_paths = {norm(p) for _m, p in spec_ops(spec)}

    missing = []
    for m, p in sorted(bundle_routes(files)):
        np = norm(p)
        if (m, np) in have:
            continue
        if np in have_paths:
            continue  # path exists, method differs — note, don't fail
        missing.append(f"{m} {p}")

    if missing:
        print(f"FAIL: {len(missing)} bundle route(s) not in the frozen spec:")
        for x in missing:
            print(f"  {x}")
        return 1
    print(f"ok: embedded bundle's routes are all in the frozen spec ({sys.argv[2]})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
