#!/usr/bin/env python3
"""Rule 72 — every shared operation with a per-pin request body must be handled.

`/api/*` operations exist in BOTH frozen contracts. Most are byte-identical, but
a few declare different request bodies (1.19.5 vs 2.2.5), and a handler written
against one pin silently 400s a client whose own contract says its body is
valid. That is how the v2 permission reply rejected `{reply}` (1.19.5) while
accepting only `{decision}` (2.2.5), and how the v2 prompt rejected the nested
`{prompt: {text}}` form.

This checker derives the divergence set from the two vendored contracts and
requires each row to be declared in HANDLED with the reason it is safe. Adding
or changing a shared body without updating this table fails the build, so the
class cannot drift silently again.

Usage: gen-shared-bodies.py [--check] [--selftest]
Exit 0 when the committed table matches the contracts; 1 otherwise.
"""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
V1 = ROOT / "crates/ocserve-core/assets/openapi/v1.json"
V2 = ROOT / "crates/ocserve-core/assets/openapi/v2.json"
METHODS = ("get", "post", "put", "patch", "delete")

# Every shared operation whose request body differs between the pins, with how
# the handler covers both. This table is the reviewable claim; the checker
# fails when the contracts change without it (missing OR stale row).
HANDLED = {
    ("POST", "/api/session"): (
        "v2 adds optional metadata/parentID/permissions/title; "
        "create_session_record reads every key optionally and ignores the rest"
    ),
    ("POST", "/api/session/{sessionID}/prompt"): (
        "v2 flattens text/files/agents/skills; the decoder lifts the 1.19.5 "
        "nested {prompt:{...}} form onto the flat shape (K-V2-PROMPT-BODY)"
    ),
    ("POST", "/api/session/{sessionID}/compact"): (
        "handler takes no body; both pins' bodies are ignored by construction"
    ),
    ("POST", "/api/integration/{integrationID}/connect/key"): (
        "handler takes no body; v2's extra `answer` is ignored by construction"
    ),
    ("POST", "/api/integration/{integrationID}/connect/oauth"): (
        "handler takes no body (honest 401, D-OAUTH); both shapes are ignored"
    ),
    ("POST", "/api/session/{sessionID}/permission/{requestID}/reply"): (
        "1.19.5 requires {reply}, 2.2.5 requires {decision}; the handler accepts "
        "either key (D-V2-PERM-REPLY-BODY)"
    ),
}


def resolve(node, root, stack=()):
    if isinstance(node, dict):
        ref = node.get("$ref")
        if isinstance(ref, str) and ref.startswith("#/"):
            if ref in stack:
                return {}
            target = root
            for seg in ref[2:].split("/"):
                target = target[seg]
            return resolve(target, root, stack + (ref,))
        return {k: resolve(v, root, stack) for k, v in node.items()}
    if isinstance(node, list):
        return [resolve(x, root, stack) for x in node]
    return node


def body_props(op, root):
    rb = op.get("requestBody")
    if not rb:
        return None
    schema = rb.get("content", {}).get("application/json", {}).get("schema")
    if not schema:
        return None
    schema = resolve(schema, root)
    if "properties" in schema:
        return sorted(schema["properties"])
    if "allOf" in schema:
        props = set()
        for part in schema["allOf"]:
            props.update(part.get("properties", {}))
        return sorted(props)
    return []


def divergences(v1, v2):
    rows = []
    for path, ops in v1["paths"].items():
        if path not in v2["paths"]:
            continue
        for method, op in ops.items():
            if method not in METHODS:
                continue
            op2 = v2["paths"][path].get(method)
            if not op2:
                continue
            a = body_props(op, v1)
            b = body_props(op2, v2)
            if a is None and b is None:
                continue
            if a == b:
                continue
            rows.append((method.upper(), path, a, b))
    return sorted(rows)


def check(rows):
    problems = []
    declared = set(HANDLED)
    actual = {(m, p) for m, p, _, _ in rows}
    for key in sorted(actual - declared):
        problems.append(f"undeclared shared-body divergence: {key[0]} {key[1]}")
    for key in sorted(declared - actual):
        problems.append(f"stale HANDLED row (no divergence anymore): {key[0]} {key[1]}")
    return problems


def main() -> int:
    v1 = json.load(open(V1, encoding="utf-8"))
    v2 = json.load(open(V2, encoding="utf-8"))
    rows = divergences(v1, v2)

    if "--selftest" in sys.argv:
        planted = rows + [("POST", "/api/planted/divergence", ["a"], ["b"])]
        if not check(planted):
            print("FAIL: planted undeclared divergence was not detected", file=sys.stderr)
            return 1
        print("ok (planted undeclared divergence detected)")
        return 0

    problems = check(rows)
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        return 1
    print(f"ok ({len(rows)} shared operations have different bodies; all declared)")
    for m, p, a, b in rows:
        print(f"  {m:5} {p}")
        print(f"        v1={a}")
        print(f"        v2={b}")
        print(f"        -> {HANDLED[(m, p)]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
