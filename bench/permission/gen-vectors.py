#!/usr/bin/env python3
"""Generate permission oracle vectors from a running freeze server.

`POST /api/session/{id}/permission` is a PURE permission oracle: it returns
`{id, effect}` with no LLM, no tool execution, no side effects beyond a
pending ask on `ask`. It evaluates the session agent's ruleset. This script
drives a committed matrix of (agent, action, resource) → effect through freeze
and records the vectors, which `permission-replay.py` later asserts against
ocserve. That is black-box differential testing (McKeeman 1998) applied to
authorization semantics — the behavioural analogue of spec-validate.py.

Usage: gen-vectors.py <freeze-base-url> <out.json> [agent...]
The vectors are generated under a FIXTURE config (see permission-check.sh) so
they are config-independent.
"""
import json
import sys
import urllib.request

# The matrix: (agent, action, resources). Chosen to exercise each rule family,
# the edit/read aliases, wildcard matching, and the default-ask fallback.
#
# SCOPE (2026-10-09): the matrix covers the CONFIG-DRIVEN flat-action model
# (edit/read/bash/webfetch/todowrite/external_directory/glob/grep), where
# freeze's V2 oracle and ocserve agree. It deliberately EXCLUDES V1 built-in
# defaults absent from V2's model (doom_loop, question, plan_*): freeze's V2
# oracle returns `allow` for those while its own V1 tool loop asks/denies —
# an upstream V1-vs-V2 inconsistency (divergence D-PERM-V2-MODEL, PLAN §17).
# ocserve reports its real (V1) tool-loop behavior, which is self-consistent.
MATRIX = [
    ("build", "read", ["src/main.rs"]),
    ("build", "read", ["secrets.env"]),
    ("build", "read", [".env.local"]),
    ("build", "read", [".env.example"]),
    ("build", "edit", ["src/main.rs"]),
    ("build", "edit", ["/etc/passwd"]),
    ("build", "bash", ["git status"]),
    ("build", "bash", ["rm -rf /"]),
    ("build", "glob", ["**/*.rs"]),
    ("build", "grep", ["TODO"]),
    ("build", "webfetch", ["https://example.com"]),
    ("build", "todowrite", ["*"]),
    ("build", "external_directory", ["/etc/*"]),
    ("build", "unknown_tool_xyz", ["*"]),
    ("plan", "edit", ["src/main.rs"]),
    ("plan", "bash", ["ls"]),
    ("general", "edit", ["src/main.rs"]),
    ("general", "read", ["src/main.rs"]),
]


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
    base, out = sys.argv[1].rstrip("/"), sys.argv[2]
    # WARM-UP BARRIER (critical, 2026-10-09): freeze's agent registry is
    # populated lazily, so the very first permission evaluations on a freshly
    # booted server hit `missingAgentPermissions` (deny-all) and then flip to
    # the real ruleset — the oracle is non-deterministic during warm-up. Drive
    # throwaway evaluations until the effect stabilizes before recording.
    warm = _new_session(base)
    last = None
    stable = 0
    for _ in range(50):
        got = post(base, warm, "build", "edit", ["src/main.rs"])
        if got == last:
            stable += 1
            if stable >= 5:
                break
        else:
            stable = 0
        last = got

    # one session per agent (some agents may not exist → session uses default)
    sids = {}
    for agent, *_ in MATRIX:
        if agent not in sids:
            sids[agent] = _new_session(base, f"perm-{agent}")

    vectors = []
    for agent, action, resources in MATRIX:
        effect = post(base, sids[agent], agent, action, resources)
        vectors.append(
            {"agent": agent, "action": action, "resources": resources, "effect": effect}
        )

    # Stability gate: re-run every vector once; a differing second read means
    # the server has not settled and the vectors would encode noise.
    unstable = []
    for v in vectors:
        again = post(base, sids[v["agent"]], v["agent"], v["action"], v["resources"])
        if again != v["effect"]:
            unstable.append((v, again))
    if unstable:
        for v, again in unstable:
            print(
                f"  UNSTABLE {v['agent']} {v['action']} {v['resources']}: "
                f"{v['effect']} then {again}",
                file=sys.stderr,
            )
        sys.exit("gen-vectors: oracle not deterministic — warm longer / investigate")

    doc = {
        "source": f"generated from {base} by gen-vectors.py (warm-up gated)",
        "note": "permission oracle vectors — never hand-edit",
        "vectors": vectors,
    }
    with open(out, "w") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
    print(f"wrote {out}: {len(vectors)} vectors (determinism verified)")


def _new_session(base, title="perm-warm") -> str:
    req = urllib.request.Request(
        f"{base}/session",
        data=json.dumps({"title": title}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    return json.loads(urllib.request.urlopen(req, timeout=10).read())["id"]


if __name__ == "__main__":
    main()
