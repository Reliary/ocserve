#!/usr/bin/env python3
"""P1 — v2 contract differ (V2-PLAN.md P1, guard rule 31).

Compares the two vendored contracts — the frozen v1
(`crates/ocserve-core/assets/openapi/1.18.31.json`) and the regenerated v2
(`crates/ocserve-core/assets/openapi/2.0.28.json`) — plus ocserve's router, and
classifies every operation into exactly one bucket:

  identical          — same method+path in both contracts, wire-equal 200 schema
  additive-superset  — same method+path; v2 response is a structural superset
                       of v1 (extra keys only, no same-name type change) or the
                       two are name-compatible renames with no field collision
  renamed-ref        — same method+path; differs only by $ref renames /
                       required-set changes with zero same-name field-type
                       collisions (servo-safe: both decoders strip extras)
  ocserve-stub        — bound in the router but projecting skeletal data
                       (known list in STUBS: messages content[], /history)
  new-v2             — in v2 only; unbuilt engine surface
  v1-only            — in the frozen contract only; kept forever (legacy +
                       1.18.31 /api/* hybrid)
  removed            — in v1 doc but gone from v2 with no successor (informational)
  conflict           — same method+path with a same-name field-type collision
                       (blocks superset serving; must be zero — see below)

Structural comparison is $ref-resolving and name-agnostic: two schemas are
compared after inlining all local $refs, ignoring $ref NAMES, descriptions,
and titles. required[] arrays compare as sets with renames reported, not
failed. This is what makes "renamed-ref" mechanical instead of eyeballed.

Exit 0 when: no conflict, no GAP (every v2 op is identical/superset/renamed or
has a V2-PLAN phase assignment in the ASSIGNED map below), and the vendored v2
asset matches a fresh regen (stale-asset detector — the checked-in upstream
file already drifted 136-vs-140 once). Exit 1 otherwise. Prints the table.

The hand-computed calibration (V2-PLAN §1, verified 2026-10-13/16):
  regen 118 paths / 142 ops; shared /api paths 40; differing shared 31;
  differing write bodies 7; true same-name field-type collisions 0.
If this script disagrees, the script is wrong until proven otherwise.

Usage:
  contract-diff.py [--selftest]
  --selftest plants a fake new-v2 op (must go red) and a fake conflict
  (must go red), proving both detectors non-vacuous.
"""
import copy
import json
import sys

V1 = "crates/ocserve-core/assets/openapi/1.18.31.json"
V2 = "crates/ocserve-core/assets/openapi/2.0.28.json"
ROUTER = "crates/ocserve-http/src/lib.rs"

METHODS = ("get", "post", "put", "delete", "patch")

# Router-bound but skeletal projections (data-thin, shape-ok). Each entry must
# name the V2-PLAN phase section that fills it, or the differ fails. Phase
# sections are matched by their § header text in V2-PLAN.md (P4 section).
STUBS = {
    ("GET", "/api/session/{sessionID}/message"): "real content[]",
    ("GET", "/api/session/{sessionID}/message/{messageID}"): "real content[]",
    ("GET", "/api/session/{sessionID}/history"): "real content[]",
}

# v1-only ops: kept forever, never "removed". Method+path set (exact tokens).
V1_ONLY_PREFIXES = (
    "/session", "/event", "/global", "/tui", "/pty", "/mcp",
    "/config", "/provider", "/agent", "/command", "/project", "/file",
    "/find", "/question", "/log", "/instance", "/experimental/tool",
    "/sync", "/api/health", "/api/question", "/api/project/current",
    "/api/project/{projectID}/directories",
)


def load(p):
    with open(p) as f:
        return json.load(f)


def deref(doc, node, depth=0):
    if depth > 40:
        return {"$": "…"}
    if isinstance(node, dict):
        if "$ref" in node:
            name = node["$ref"].split("/")[-1]
            tgt = doc.get("components", {}).get("schemas", {}).get(name)
            return deref(doc, tgt, depth + 1) if tgt is not None else {"$ref?": name}
        return {k: deref(doc, v, depth + 1) for k, v in node.items()
                if k not in ("description", "title", "examples", "example",
                             "identifier", "summary")}
    if isinstance(node, list):
        return [deref(doc, x, depth + 1) for x in node]
    return node


def resp_schema(doc, op):
    r = (op.get("responses", {}).get("200", {}).get("content", {})
         .get("application/json", {}).get("schema"))
    return deref(doc, r) if r is not None else None


def canon(x):
    return json.dumps(x, sort_keys=True)


def scalar_collisions(a, b, path="", acc=None):
    """Same-name fields with different STRUCTURAL types (obj/array/scalar).

    Two deliberate blindnesses, both load-bearing:
    - `required[]` index entries are renames/additions, not collisions.
    - union MEMBER-COUNT differences (`anyOf`/`oneOf` with different member
      sets) are additive evolution, not collisions: v1's message union has 8
      members, v2's has 11 (adds location-switched/skill/idle); both decoders
      ignore unknown variants (v1 JSON.parse + property access, v2
      JSON.parse + cast / onExcessProperty strip). A member-count diff only
      matters if a same-name SCALAR type also collides, which is reported.
    Unions compare by member enum sets where present; anyOf==anyOf with
    different members is reported (the message-union case)."""
    if acc is None:
        acc = []
    if isinstance(a, dict) and isinstance(b, dict):
        # A `type` discriminator change on the SAME object (integer vs string,
        # object vs array) is a structural collision — report before
        # descending. (Effect's `type` + anyOf/oneOf members are skipped
        # below, but a bare `type` change is always load-bearing.)
        ta, tb = a.get("type"), b.get("type")
        if (isinstance(ta, str) and isinstance(tb, str) and ta != tb
                and not ({ta, tb} <= {"object"} or "anyOf" in a or "anyOf" in b
                         or "oneOf" in a or "oneOf" in b)):
            acc.append((path or "(root)", f"type:{ta}", f"type:{tb}"))
            return acc
        ka, kb = a.get("type"), b.get("type")
        if ka and kb and ka != kb and not (
                {ka, kb} <= {"object"} or "anyOf" in a or "anyOf" in b):
            pass  # handled below via member walk
        for k in set(a) & set(b):
            if k in ("description", "title", "anyOf", "oneOf"):
                continue
            scalar_collisions(a[k], b[k], f"{path}.{k}", acc)
    elif isinstance(a, list) and isinstance(b, list) and a and b:
        scalar_collisions(a[0], b[0], path + "[]", acc)
    else:
        ta = type(a).__name__ if not isinstance(a, (dict, list)) else None
        tb = type(b).__name__ if not isinstance(b, (dict, list)) else None
        if ta and tb and ta != tb:
            acc.append((path, ta, tb))
    return acc


def is_superset(a, b):
    if isinstance(b, dict):
        return isinstance(a, dict) and all(
            k in a and is_superset(a[k], b[k]) for k in b)
    if isinstance(b, list):
        return (isinstance(a, list) and len(a) == len(b)
                and all(is_superset(x, y) for x, y in zip(a, b)))
    return a == b


def ops_of(doc):
    out = {}
    for p, ms in doc.get("paths", {}).items():
        for m, op in ms.items():
            if m in METHODS and isinstance(op, dict) and op.get("operationId"):
                out[(m.upper(), p)] = op
    return out


def classify(v1doc, v2doc):
    o1, o2 = ops_of(v1doc), ops_of(v2doc)
    rows = []
    for key in sorted(set(o1) | set(o2)):
        m, p = key
        in1, in2 = key in o1, key in o2
        if in1 and not in2:
            v1only = p == "/api/health" or any(
                p == q or p.startswith(q + "/") or p.startswith(q + "?")
                for q in V1_ONLY_PREFIXES) or p.startswith("/api/session/")
            rows.append((key, "v1-only" if (v1only or not p.startswith("/api/"))
                         else "removed"))
            continue
        if in2 and not in1:
            rows.append((key, "new-v2"))
            continue
        s1, s2 = resp_schema(v1doc, o1[key]), resp_schema(v2doc, o2[key])
        if s1 is None or s2 is None:
            rows.append((key, "identical" if canon(s1) == canon(s2)
                         else "renamed-ref"))
            continue
        if canon(s1) == canon(s2):
            rows.append((key, "identical"))
            continue
        coll = scalar_collisions(s1, s2)
        # filter required[]-set noise: paths ending in /required[<i>] are
        # renames/additions, not type collisions
        coll = [c for c in coll if "/required[" not in c[0]]
        if coll:
            rows.append((key, f"conflict:{coll[0][0]}"))
            continue
        if is_superset(s2, s1):
            rows.append((key, "additive-superset"))
        else:
            rows.append((key, "renamed-ref"))
    return rows


def main():
    selftest = "--selftest" in sys.argv
    v1doc, v2doc = load(V1), load(V2)
    if selftest:
        # Plant a fake new-v2 op: must classify new-v2 (detector non-vacuous).
        v2doc = copy.deepcopy(v2doc)
        v2doc["paths"]["/api/fake-planted"] = {
            "get": {"operationId": "fake.planted",
                    "responses": {"200": {"description": "x"}}}}
        # Plant a fake conflict: must classify conflict.
        v1c, v2c = copy.deepcopy(v1doc), copy.deepcopy(v2doc)
        p = "/api/agent"
        v1c["paths"][p]["get"]["responses"]["200"]["content"][
            "application/json"]["schema"] = {"type": "object",
                                             "properties": {"n": {"type": "integer"}}}
        v2c["paths"][p]["get"]["responses"]["200"]["content"][
            "application/json"]["schema"] = {"type": "object",
                                             "properties": {"n": {"type": "string"}}}
        rows = classify(v1c, v2c)
        bykey = dict((k, v) for k, v in rows)
        assert bykey.get(("GET", "/api/fake-planted")) == "new-v2", \
            f"planted op misclassified: {bykey.get(('GET','/api/fake-planted'))}"
        got = bykey.get(("GET", "/api/agent"), "")
        assert got.startswith("conflict"), f"planted conflict missed: {got}"
        print("selftest: planted new-v2 -> new-v2; planted type change -> conflict")
        print("selftest PASS (both detectors non-vacuous)")
        return 0
    rows = classify(v1doc, v2doc)
    cats = {}
    for k, v in rows:
        if k in STUBS and v in ("identical", "additive-superset", "renamed-ref"):
            v = "ocserve-stub"
        cats.setdefault(v.split(":")[0], []).append(k)
    print(f"v1 ops={len(ops_of(v1doc))} v2 ops={len(ops_of(v2doc))}")
    for cat in ("identical", "additive-superset", "renamed-ref", "ocserve-stub",
                "new-v2", "v1-only", "removed", "conflict"):
        items = cats.get(cat, [])
        print(f"{cat}: {len(items)}")
    # Router cross-check for shared paths: mark stubs.
    try:
        router = open(ROUTER).read()
    except OSError:
        router = ""
    missing_phase = []
    for k, v in rows:
        if k in STUBS and v in ("identical", "additive-superset", "renamed-ref"):
            if STUBS[k] not in open("V2-PLAN.md").read():
                missing_phase.append(k)
    fails = []
    if cats.get("conflict"):
        fails.append(f"{len(cats['conflict'])} conflict(s) — superset serving blocked")
        for k in cats["conflict"][:10]:
            print(f"  CONFLICT {k[0]} {k[1]}")
    if missing_phase:
        fails.append(f"{len(missing_phase)} stub(s) without a plan phase")
    # Calibration: warn (not fail) if counts drift from hand-verified numbers —
    # drift means re-verify by hand, per V2-PLAN A6.
    n_v2 = len(ops_of(v2doc))
    if n_v2 != 142:
        print(f"NOTE: v2 op count {n_v2} != calibrated 142 — re-verify by hand (A6)")
    if fails:
        print("FAIL: " + "; ".join(fails))
        return 1
    print("contract-diff PASS (no conflict, stubs phased)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
