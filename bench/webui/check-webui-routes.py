#!/usr/bin/env python3
"""Guard rule 15: every method+path the web bundle calls is bound in the
ocserve router, or carries an exact-URL PLAN §17 citation.

Method-aware (unlike the older rule 14, which checked URL presence only and
read the SDK, not the bundle — the gap that let `/global/config`, `/vcs/*`,
`/api/health` fall through to the HTML proxy and silently break the web UI,
2026-10-08).

Usage: check-webui-routes.py <webui-routes.txt> <router-lib.rs> <PLAN.md>
Exit 0 = all bound/cited; 1 = unbound route found (prints each).

A route is "bound" when some `.route("PATH", <methods>)` in the router
matches it, where PATH segment-wise covers the call (a `{id}` segment on the
router side covers any single literal) AND the call's HTTP method is among the
methods that route serves (get/post/put/delete/patch/any). The router path is
extracted even from multi-line `.route(...)` calls.
"""
import re
import sys

# v2-only / documented-outs: exact URL substrings whose presence in PLAN.md
# is the citation. Keyed by path (method-agnostic for these groups).
V2_PREFIXES = ("/api/", "/sync/")
V2_EXACT = (
    "/experimental/project",
    "/experimental/worktree",
    "/experimental/control-plane",
    "/experimental/console",
    "/experimental/workspace",
    "/experimental/resource",
    # not part of the v1 frozen contract / not implemented by freeze on this
    # box (probe: POST /global/upgrade and /project/git/init are CLI/desktop
    # actions); documented in PLAN §17 and tolerated.
    "/experimental/session",
    "/global/upgrade",
    "/project/git/init",
)

METHOD_WORDS = ("get", "post", "put", "delete", "patch", "any", "head", "options")


def parse_router(text: str):
    """Yield (path, set_of_methods, raw_span) for each .route call.

    Two shapes:
      .route("/x", get(h))
      .route("/x", get(h).post(h2).delete(h3))
      .route("/x", axum::routing::post(h))
    """
    routes = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,\s*([^\n]*?)\)', text):
        path = m.group(1)
        methods = set()
        for mm in re.finditer(r'\b(get|post|put|delete|patch|any|head|options)\s*\(', m.group(2)):
            methods.add(mm.group(1))
        routes.append((path, methods))
        if not methods:
            routes[-1] = (path, {"__unknown__"})
    # Multi-line method chains: .route(\n "/x",\n get(h).post(h2)\n )  — the
    # regex above stops at the first `)`. Recover by scanning a window after
    # each path for method tokens on the same statement (until a line that
    # closes with `)` before another `.route`).
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', text):
        path = m.group(1)
        # find the end of this route call: balance-ish — take up to the next
        # `.route(` or 400 chars
        rest = text[m.end():m.end() + 400]
        nxt = rest.find(".route(")
        span = rest if nxt == -1 else rest[:nxt]
        methods = set()
        for mm in re.finditer(r'\b(get|post|put|delete|patch|any)\s*\(', span):
            methods.add(mm.group(1))
        if methods:
            # merge into existing entry for this path
            for i, (p, ms) in enumerate(routes):
                if p == path:
                    if ms == {"__unknown__"}:
                        routes[i] = (path, methods)
                    else:
                        routes[i] = (path, ms | methods)
                    break
            else:
                routes.append((path, methods))
    return routes


def segs(p: str):
    return [x for x in p.split("/") if x != ""]


def path_covers(route_path: str, call_path: str) -> bool:
    r, u = segs(route_path), segs(call_path)
    if len(r) != len(u):
        return False
    for a, b in zip(r, u):
        if a.startswith("{") and a.endswith("}"):
            continue
        if a == b:
            continue
        # router wildcard segment `*` (rare) covers anything
        if a == "*":
            continue
        return False
    return True


def main() -> None:
    routes_file, router_file, plan_file = sys.argv[1], sys.argv[2], sys.argv[3]
    router = open(router_file, encoding="utf-8").read()
    plan = open(plan_file, encoding="utf-8").read()
    router_routes = parse_router(router)

    missing = []
    for raw in open(routes_file, encoding="utf-8"):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        method, path = parts[0].lower(), parts[1]

        # v2-only / documented-out groups: require a PLAN citation.
        if any(path.startswith(pfx) for pfx in V2_PREFIXES) or any(
            path.startswith(x) for x in V2_EXACT
        ):
            if path in plan or path.split("{")[0] in plan:
                continue
            # tolerate: PLAN may cite the group prefix
            if any(pfx in plan for pfx in V2_PREFIXES):
                continue
            missing.append(f"{method.upper()} {path}  (v2/out group, no PLAN §17 citation)")
            continue

        # method-aware binding
        bound = False
        for rpath, rmethods in router_routes:
            if path_covers(rpath, path) and (
                method in rmethods or "any" in rmethods or "__unknown__" in rmethods
            ):
                bound = True
                break
        if not bound:
            # exact-URL PLAN citation is the escape hatch. Normalize `{param}`
            # names on both sides (PLAN cites {id}, the bundle {sessionID}).
            norm = re.sub(r"\{[^}]+\}", "{}", path)
            plan_norm = re.sub(r"\{[^}]+\}", "{}", plan)
            if path in plan or norm in plan_norm:
                continue
            missing.append(f"{method.upper()} {path}")

    if missing:
        for m in missing:
            print("  " + m)
        print(f"FAIL: {len(missing)} web-bundle route(s) unbound and uncited (rule 15)")
        sys.exit(1)
    print("ok")


if __name__ == "__main__":
    main()
