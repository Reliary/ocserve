#!/usr/bin/env python3
"""P1 — generated API coverage guard (replaces guard rules 14 + 15).

The frozen opencode 1.18.31 server exposes its complete OpenAPI 3.1.0 contract
at `GET /doc` (vendored at bench/openapi/1.18.31.json). This script
cross-references every operation in that contract against the ocserve router and
reports, per operation:

  implemented  — bound in the router (method + path)
  cited-out    — the exact method+path appears in PLAN.md (documented deferral)
  GAP          — neither: an unbound operation with no citation

Exit 0 when every operation is implemented or cited (no GAP); exit 1 otherwise.
Writes a human scoreboard to bench/openapi/coverage.md on every run.

Why this replaces hand lists: the contract is machine-readable and regenerated
from the upstream tag (`scripts/extract-spec.sh`), so coverage cannot drift the
way three hand-maintained route lists did (the /pty/shells and
/session/{id}/diff gaps, 2026-10-08).

Usage:
  check-coverage.py <spec.json> <router-lib.rs> <PLAN.md> [--write]
"""
import json
import re
import sys

METHODS = ("get", "post", "put", "delete", "patch")
ROUTER_METHODS = ("get", "post", "put", "delete", "patch", "any", "head", "options")


def parse_router(text):
    """Return list of (path, method_set). Handles multi-line .route(...) and
    `get(h).post(h2)` chains and `axum::routing::post(h)` forms."""
    out = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,\s*', text):
        path = m.group(1)
        # take the span up to the matching close of this .route( call
        depth = 1
        i = m.end()
        while i < len(text) and depth:
            c = text[i]
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            i += 1
        span = text[m.end():i]
        methods = set()
        for word in ROUTER_METHODS:
            # `get(` or `axum::routing::get(` or `.get(`
            if re.search(rf'(?:^|[^A-Za-z_]){word}\s*\(', span):
                methods.add(word if word not in ("any",) else "any")
        out.append((path, methods))
    return out


def segs(p):
    return [x for x in p.split("/") if x != ""]


def covers(route, url):
    r, u = segs(route), segs(url)
    if len(r) != len(u):
        return False
    for a, b in zip(r, u):
        if a == "{...}" or (a.startswith("{") and a.endswith("}")):
            continue
        if a != b:
            return False
    return True


def method_ok(route_methods, method):
    if not route_methods:
        return False
    if "any" in route_methods:
        return True
    return method in route_methods


# A citation is legitimate only when the EXACT route token appears as a whole
# token — i.e. the character after `METHOD /path` must not be a continuation of
# the path (a path-segment char: alphanumeric, `_`, `-`, `/`, `{`, `.`, `~`,
# `%`). This blocks `POST /mcp` matching the interior of `POST /mcp/{name}/...`
# while still allowing a citation followed by prose, punctuation, backticks,
# whitespace, `|`, `,`, `)`, etc. A trailing `/` explicitly continues the path.
PATH_CONTINUATION = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-/{.~%")


def is_cited(key, plan):
    """True iff `key` (e.g. `POST /mcp`) appears in `plan` as a whole route
    token, not as a prefix of a longer route."""
    start = 0
    while True:
        i = plan.find(key, start)
        if i < 0:
            return False
        j = i + len(key)
        # token boundary: end of text, or a non-path-continuation char.
        # The method token also needs a boundary before it (avoid matching the
        # middle of e.g. `XPOST /mcp`); the method is preceded by a space or
        # backtick in practice.
        if j >= len(plan) or plan[j] not in PATH_CONTINUATION:
            return True
        start = i + 1


def main():
    spec_file, router_file, plan_file = sys.argv[1], sys.argv[2], sys.argv[3]
    write = "--write" in sys.argv
    spec = json.load(open(spec_file, encoding="utf-8"))
    router_text = open(router_file, encoding="utf-8").read()
    plan = open(plan_file, encoding="utf-8").read()
    routes = parse_router(router_text)

    impl, cited, gaps = [], [], []
    for path, ops in spec["paths"].items():
        for entry in ops:
            method = entry.lower()
            if method not in METHODS:
                continue
            bound = any(
                covers(rp, path) and method_ok(rm, method) for rp, rm in routes
            )
            key = f"{method.upper()} {path}"
            if bound:
                impl.append(key)
            elif is_cited(key, plan):
                # A citation must be the EXACT `METHOD /path` as a whole route
                # token — not a substring of a longer route. The earlier
                # `key in plan` substring test vacuously cited `POST /mcp`
                # because it is a prefix of the cited `POST /mcp/{name}/connect`
                # (2026-10-09; the same vacuous-pass class the guard exists to
                # prevent). See is_cited().
                cited.append(key)
            else:
                gaps.append(key)

    total = len(impl) + len(cited) + len(gaps)
    lines = [
        "# API coverage (generated — do not edit)",
        "",
        f"Source: `bench/openapi/1.18.31.json` ({len(spec['paths'])} paths).",
        "Generated by `bench/openapi/check-coverage.py`.",
        "",
        f"- implemented: **{len(impl)}**",
        f"- cited-out:   **{len(cited)}**",
        f"- GAP:         **{len(gaps)}**",
        f"- total ops:   {total}",
        "",
    ]
    if gaps:
        lines += ["## GAP — unbound and uncited", ""]
        lines += [f"- `{g}`" for g in sorted(gaps)]
        lines += [""]
    if cited:
        lines += ["## cited-out (PLAN.md)", ""]
        lines += [f"- `{c}`" for c in sorted(cited)]
        lines += [""]
    text = "\n".join(lines)
    if write:
        open("bench/openapi/coverage.md", "w", encoding="utf-8").write(text)

    print(f"coverage: implemented={len(impl)} cited={len(cited)} gap={len(gaps)} total={total}")
    for g in sorted(gaps):
        print(f"  GAP {g}")
    return 1 if gaps else 0


if __name__ == "__main__":
    sys.exit(main())
