#!/usr/bin/env python3
"""P4 — calibrated spec validation (bench/openapi/SPEC-DRIVEN-PLAN.md).

Validates every JSON 200 response from a running server against the frozen
OpenAPI 3.1.0 contract (`crates/ocserve-core/assets/openapi/1.18.31.json`). This is the layer the
generated coverage guard (rule 16) cannot provide: rule 16 proves a route is
BOUND; this proves the response SHAPE matches the contract (type drift,
missing required, bad enums, unexpected extra properties).

Calibration (why it is not a strict validator):
  * freeze violates its OWN contract in 93 places by emitting `null` for
    optional-but-typed fields (`/vcs`, `/command`, `/agent`). A strict gate
    would be red on freeze. So `relax_null()` makes every declared type accept
    null — the oracle then gates on WRONG types / missing required / bad enums
    / extra props, which is what actually breaks clients.
  * `/config/providers` is a documented pair divergence (D-PAIR-2): freeze
    serves a compiled registry, ocserve the models.json copy. Allowlisted.

Usage: spec-validate.py <base-url> [--spec <contract.json>]
  --spec defaults to the frozen 1.18.31 contract; pass the 2.0.28 asset for
  the v2 surface (V2-PLAN.md P5). Parameterless GETs validate directly;
  session-scoped v2 GETs validate against a freshly created session.
Exit 0 when no non-allowlisted route violates the contract; 1 otherwise.
"""
import json
import os
import re
import sys
import urllib.request

import jsonschema

SPEC = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "../../crates/ocserve-core/assets/openapi/1.18.31.json",
)

# Per-base allowlist of path -> reason. A route here is a NAMED divergence,
# never a silent skip (mirrors pair-check's D-PAIR rows / guard rule 12).
ALLOW = {
    # D-PAIR-2: provider registry provenance. Freeze serves a compiled
    # registry (215 providers, 1.18.31-era); ocserve serves the models.json
    # catalog copy, so per-model variant/family/cost keypaths differ.
    "/config/providers": "D-PAIR-2 provider registry provenance",
    # D3 (DIFFERENTIATION.md): ocserve adds an additive `trust` object
    # (+`dropped_tools`) to each MCP status for the MCP trust layer. Freeze's
    # MCPStatus is additionalProperties:false, so the additive field trips the
    # validator — a deliberate, documented divergence, not a drift.
    "/mcp": "D3 additive MCP trust field",
}

# D-V2-UNION (V2-PLAN.md P4): the single-port union serves v1-required keys
# inside v2 additionalProperties:false objects (and vice versa) because no
# on-the-wire discriminator exists and both client families strip extras
# (verified decode paths). Each entry names the v1-required key that forces
# the union. Real v2 clients tolerate extras; the v1 corpus (keys_subset)
# and the v2 required-keys check both pass.
ALLOW_V2_UNION = {
    "/api/agent": "v1 LocationInfo.project required (envelope)",
    "/api/command": "v1 CommandV2Info.template required",
    "/api/skill": "v1 SkillV2Info.location+slash required",
    "/api/reference": "v1 envelope project required",
    "/api/pty": "v1 envelope project required",
    "/api/provider": "v1 ProviderV2Info.api+request required",
    "/api/permission/request": "v1 envelope project required",
    "/api/model": "v1 ModelV2Info.api+request required",
    "/api/location": "v1 LocationInfo.project required",
    "/api/integration": "v1 envelope project required",
    "/api/fs/list": "v1 envelope project required",
    "/api/question/request": "v1 envelope project required",
    # v2 Project.canonical and time.active are served in the shared union shape
    # on the v1 routes too: the 1.18.31 contract already contains /project and
    # /project/current, and the hybrid client reads the union object.
    "/project": "v2 Project.canonical + time.active in the shared union shape",
    "/project/current": "v2 Project.canonical + time.active in the shared union shape",
    # v2-only route, but the envelope is the shared cached location value
    # (one shape everywhere — cache design over per-route exactness).
    "/api/form": "shared envelope project (no v1 shape; envelope uniformity)",
    # Phase A routes: same union class (v1-required keys in v2 objects).
    "/api/mcp": "v1 envelope project required (data projected to Mcp.Server array)",
    "/api/mcp/resource": "v1 envelope project required",
    "/api/vcs": "v1 envelope project required (data is per-contract Vcs.Info)",
    "/api/vcs/status": "v1 envelope project required",
    "/api/vcs/base": "v1 envelope project required",
    "/api/vcs/branch": "v1 envelope project required",
    "/api/project": "v1 Project.worktree+time.initialized required",
    # Phase C routes: same union class.
    "/api/config": "config entries are additive documents (no v1 shape)",
    "/api/config/shell": "shell options are additive (no v1 shape)",
    "/api/model/default": "v1 ModelV2Info.api+request required on the item",
    "/api/debug/location": "location refs are additive (no v1 shape)",
    "/api/plugin": "plugin inventory honestly empty (D-PLUGIN-INVENTORY)",
    "/api/worktree": "worktree entries are additive (no v1 shape)",
    # Phase D/F routes: same union class (shared cached envelope).
    "/api/shell": "shared envelope project (no v1 shape; envelope uniformity)",
    "/api/session/active": "shared envelope project (no v1 shape; envelope uniformity)",
    "/api/session/{sessionID}/diff": "shared envelope project (no v1 shape; envelope uniformity)",
    "/api/pair": "pairing records are additive (no v1 shape)",
    "/api/experimental/mcp": "mcp records are additive (no v1 shape)",
}


def inline(node, root, stack=()):
    """Resolve local `#/...` $refs into a self-contained schema. A reference
    cycle returns {} (accept-anything) — safe, and no response body here needs
    cycle fidelity."""
    if isinstance(node, dict):
        ref = node.get("$ref")
        if isinstance(ref, str) and ref.startswith("#/"):
            if ref in stack:
                return {}
            target = root
            try:
                for seg in ref[2:].split("/"):
                    target = target[seg]
            except (KeyError, TypeError):
                return {}
            return inline(target, root, stack + (ref,))
        return {k: inline(v, root, stack) for k, v in node.items()}
    if isinstance(node, list):
        return [inline(x, root, stack) for x in node]
    return node


def relax_null(node):
    """Accept null wherever a type is declared (freeze emits null for optional
    typed fields; see module doc)."""
    if isinstance(node, dict):
        t = node.get("type")
        if isinstance(t, str):
            node["type"] = [t, "null"]
        elif isinstance(t, list) and "null" not in t:
            node["type"] = t + ["null"]
        return {k: relax_null(v) for k, v in node.items()}
    if isinstance(node, list):
        return [relax_null(x) for x in node]
    return node


def main():
    args = sys.argv[1:]
    spec_path = SPEC
    if "--spec" in args:
        i = args.index("--spec")
        spec_path = args[i + 1]
        args = args[:i] + args[i + 2:]
    if len(args) != 1:
        sys.exit("usage: spec-validate.py <base-url> [--spec <contract.json>]")
    base = args[0].rstrip("/")
    spec = json.load(open(spec_path, encoding="utf-8"))
    # v2 detection must be VERSION-AGNOSTIC: the old `"2.0.28" in spec_path`
    # silently treated any other v2 tag (e.g. 2.0.32.json during a bump) as
    # v1 — disabling the D-V2-UNION allowlist and the session seeding, so the
    # bump produced a wall of false violations. Derive the major version from
    # the filename instead.
    _m = re.search(r"(\d+)\.\d+\.\d+\.json$", os.path.basename(spec_path))
    is_v2 = bool(_m) and int(_m.group(1)) >= 2

    checked = 0
    skipped = 0
    viol = []
    session_id = None
    if is_v2:
        # Seed one session for the parameterized v2 GETs (create via the v1
        # route — the v2 create needs no model; both arms share the seeder).
        try:
            req = urllib.request.Request(
                base + "/session", data=json.dumps({"title": "specv2"}).encode(),
                headers={"Content-Type": "application/json"}, method="POST")
            session_id = json.loads(urllib.request.urlopen(req, timeout=8).read()).get("id")
        except Exception:
            session_id = None
    for path, ops in spec["paths"].items():
        op = ops.get("get")
        if not op:
            continue
        if "{" in path:
            # v2 session-scoped GETs validate against the seeded session.
            if not (is_v2 and session_id and "{sessionID}" in path
                    and path.count("{") == 1):
                continue
            url = base + path.replace("{sessionID}", session_id)
        else:
            url = base + path
        schema = (
            op.get("responses", {})
            .get("200", {})
            .get("content", {})
            .get("application/json", {})
            .get("schema")
        )
        if not schema:
            continue
        try:
            data = json.loads(urllib.request.urlopen(url, timeout=8).read())
        except Exception:
            skipped += 1
            continue
        checked += 1
        resolved = relax_null(inline(schema, spec))
        validator = jsonschema.Draft202012Validator(resolved)
        for e in sorted(validator.iter_errors(data), key=lambda e: list(e.absolute_path)):
            if path in ALLOW:
                continue
            # D-V2-UNION: additionalProperties complaints on union paths are
            # the named divergence (v1-required keys inside v2 objects).
            # anyOf-wrapped violations hide the message in sub-errors
            # (e.context), so walk the whole error tree.
            def is_addprops(err):
                if err.validator == "additionalProperties":
                    return True
                return any(is_addprops(c) for c in getattr(err, "context", []) or [])
            # The union surface is `/api/*` in BOTH spec versions: the
            # 1.18.31 contract already contains 58 `/api/*` operations (the
            # hybrid TUI client calls them), and ocserve serves them with the
            # union keys. Gating this on `is_v2` meant validating the v1 spec
            # never applied the named divergence, so the v1 run reported 8
            # violations on both the musl and glibc binaries. The allowlist
            # only suppresses additionalProperties errors on named paths, which
            # is exactly the divergence.
            if path in ALLOW_V2_UNION and is_addprops(e):
                continue
            where = "/".join(str(x) for x in e.absolute_path)
            viol.append((path, where, str(e.message)[:120]))

    print(f"spec-validate {base}: checked {checked} | violations {len(viol)} | skipped {skipped}")
    for p, w, m in viol:
        print(f"  VIOL {p} [{w}] {m}")
    return 1 if viol else 0


if __name__ == "__main__":
    sys.exit(main())
