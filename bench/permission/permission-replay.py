#!/usr/bin/env python3
"""Replay permission oracle vectors against a running ocserve (differential).

Reads the committed vectors (gen-vectors.py against freeze) and asserts
ocserve's `POST /api/session/{id}/permission` returns the SAME effect for every
(agent, action, resources) vector. A mismatch is behavioural drift the
route-binding and shape guards cannot see.

The target and freeze must run under the SAME fixture config so the vectors
are comparable (permission-check.sh guarantees it).

Usage: permission-replay.py <vectors.json> <target-base-url>
Exit 0 when every vector matches; 1 on any divergence (prints each).
"""
import json
import sys
import urllib.request


def post(base, sid, agent, action, resources):
    body = json.dumps(
        {"action": action, "resources": resources, "save": [], "agent": agent}
    ).encode()
    req = urllib.request.Request(
        f"{base}/api/session/{sid}/permission",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    d = json.loads(urllib.request.urlopen(req, timeout=10).read())
    return d["data"]["effect"]


def main():
    vectors_file, base = sys.argv[1], sys.argv[2].rstrip("/")
    doc = json.load(open(vectors_file, encoding="utf-8"))
    vectors = doc["vectors"]

    sids = {}
    for v in vectors:
        agent = v["agent"]
        if agent not in sids:
            sids[agent] = _new_session(base, f"perm-{agent}")

    # Warm-up barrier: freeze populates agent rules lazily, so the first
    # evaluations on a fresh server hit `missingAgentPermissions` (deny-all).
    # Drive throwaway evaluations until stable (matches gen-vectors.py).
    warm = _new_session(base, "perm-warm")
    last, stable = None, 0
    for _ in range(50):
        got = post(base, warm, "build", "edit", ["src/main.rs"])
        if got == last:
            stable += 1
            if stable >= 5:
                break
        else:
            stable = 0
        last = got

    diffs = []
    for v in vectors:
        got = post(base, sids[v["agent"]], v["agent"], v["action"], v["resources"])
        if got != v["effect"]:
            diffs.append((v, got))

    print(f"permission-replay {base}: {len(vectors)} vectors, {len(diffs)} divergent")
    for v, got in diffs:
        print(
            f"  DIVERGE agent={v['agent']} action={v['action']} "
            f"resources={v['resources']} want={v['effect']} got={got}"
        )
    return 1 if diffs else 0


def _new_session(base, title="perm-replay") -> str:
    req = urllib.request.Request(
        f"{base}/session",
        data=json.dumps({"title": title}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    return json.loads(urllib.request.urlopen(req, timeout=10).read())["id"]


if __name__ == "__main__":
    sys.exit(main())
