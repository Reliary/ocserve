#!/usr/bin/env python3
"""Intra-request hook order: system.transform -> chat.params -> chat.headers.

Each request round must fire the group in that order. Consecutive duplicates are
collapsed first because the 1.18.31 capture binary registers the probe twice and
dispatches both; that is a capture artefact, not a call site.

A round may legitimately be a PREFIX of the group (a request that fails before
the headers are built), but never a permutation.
"""
import json
import sys
import pathlib

GROUP = (
    "experimental.chat.system.transform",
    "chat.params",
    "chat.headers",
)


def rounds(path):
    seq = []
    for line in pathlib.Path(path).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        e = json.loads(line)
        if "marker" in e:
            continue
        h = e.get("hook")
        if h in GROUP:
            seq.append(h)
    dedup = []
    for h in seq:
        if not dedup or dedup[-1] != h:
            dedup.append(h)
    out, cur = [], []
    for h in dedup:
        if h == GROUP[0] and cur:
            out.append(cur)
            cur = []
        cur.append(h)
    if cur:
        out.append(cur)
    return out


def main():
    bad = []
    for path in sys.argv[1:]:
        for r in rounds(path):
            if not r or r != list(GROUP[: len(r)]):
                bad.append(f"{path}: {r}")
    if bad:
        print("DIFF intra-request order:", "; ".join(bad[:4]))
        return 1
    print("intra-request order: system -> params -> headers on both sides")
    return 0


if __name__ == "__main__":
    sys.exit(main())
