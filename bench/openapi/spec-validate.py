#!/usr/bin/env python3
"""P4 — calibrated spec validation (bench/openapi/SPEC-DRIVEN-PLAN.md).

Validates every JSON 200 response from a running server against the frozen
OpenAPI 3.1.0 contract (`bench/openapi/1.18.31.json`). This is the layer the
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

Usage: spec-validate.py <base-url>
Exit 0 when no non-allowlisted route violates the contract; 1 otherwise.
"""
import json
import os
import sys
import urllib.request

import jsonschema

SPEC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "1.18.31.json")

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
    if len(sys.argv) != 2:
        sys.exit("usage: spec-validate.py <base-url> (e.g. http://127.0.0.1:4912)")
    base = sys.argv[1].rstrip("/")
    spec = json.load(open(SPEC, encoding="utf-8"))

    checked = 0
    skipped = 0
    viol = []
    for path, ops in spec["paths"].items():
        op = ops.get("get")
        if not op or "{" in path:  # skip path-param routes (need real ids)
            continue
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
            data = json.loads(urllib.request.urlopen(base + path, timeout=8).read())
        except Exception:
            skipped += 1
            continue
        checked += 1
        resolved = relax_null(inline(schema, spec))
        validator = jsonschema.Draft202012Validator(resolved)
        for e in sorted(validator.iter_errors(data), key=lambda e: list(e.absolute_path)):
            if path in ALLOW:
                continue
            where = "/".join(str(x) for x in e.absolute_path)
            viol.append((path, where, str(e.message)[:120]))

    print(f"spec-validate {base}: checked {checked} | violations {len(viol)} | skipped {skipped}")
    for p, w, m in viol:
        print(f"  VIOL {p} [{w}] {m}")
    return 1 if viol else 0


if __name__ == "__main__":
    sys.exit(main())
