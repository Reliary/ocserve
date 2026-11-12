#!/usr/bin/env python3
"""Guard rule: a declared query parameter on a GET route must be referenced by
that route's handler, or carry a named divergence.

The class this closes (2026-11-11, Paseo report + two more found by this scan):
a route advertises `?cursor=` / `?order=` / `?archived=` in the frozen contract,
the handler never reads it, and the client silently gets unfiltered or
unpaginated data. Every existing gate is blind to it:

  * coverage  (rule 16/16b) checks the ROUTE exists, not its inputs
  * shape     (spec-validate) checks the RESPONSE body
  * effect    (rule 29) keys on mutating methods only
  * replay    compares recorded responses, which never varied the parameter

Usage:
  check-query-params.py <spec.json> <router.rs> [<handler.rs> ...] [--write]

Exit 1 when a declared query parameter is unreferenced and not allowlisted.
Writes bench/openapi/query-params.md on every run.

The allowlist is keyed `METHOD path param` and every entry must name a `D-`
divergence that exists in PLAN.md (rule 31 cross-checks the token).
"""
import json
import re
import sys

# Declared-but-not-implemented query parameters. Every entry needs a reason
# that is a real divergence token, never "TODO".
ALLOW = {
    # v1 `/experimental/session?directory=` scopes the listing to one
    # worktree; our store keeps root sessions across directories and the
    # clients we serve never pass it (oc-remote sends search/limit only).
    "GET /experimental/session directory": "D-EXPERIMENTAL-SESSION-DIRECTORY",
    # `/api/pty/.../location[directory]` is a bracket-nested parameter our
    # router parses as a plain query key; the TUI sends the top-level form.
    "GET /api/pty/{ptyID}/connect location[directory]": "D-PTY-TICKET-QUERY",
    "GET /pty/{ptyID}/connect directory": "D-PTY-TICKET-QUERY",
}

# `location` is the v2 request-scoping parameter. Every server is single-location
# (one worktree per process), exactly as upstream's instance model is, so the
# served `location` is the server's own directory and the query form cannot
# change it. MEASURED: the reference accepts either spelling and always echoes
# its own directory (`/api/vcs/status?location[directory]=/home` ->
# `{"location":{"directory":"/tmp"}}`), so ignoring it IS parity.
# `location` is checked structurally rather than per route: MEASURED against
# 2.2.5, `GET /api/vcs/status?location[directory]=/home` answers
# `{"location":{"directory":"/tmp"}}` — the reference echoes its OWN directory
# and the parameter never changes the answer. Every ocserve process is
# single-location for the same reason an upstream instance is, so ignoring the
# parameter IS parity. A per-route list would be a second thing to maintain.
LOCATION_PARAM = "location"
# Remaining declared-but-unimplemented parameters, each with a recorded reason.
# These are the honest "we accept the request but the value does not change the
# answer yet" set; every one is a compatibility gap a client can hit.
ALLOW.update({
    # Streaming/PTY cursors: our PTY replays from an offset internally and the
    # clients reconnect rather than resume mid-stream.
    "GET /api/pty/{ptyID}/connect cursor": "D-PTY-RESUME-CURSOR",
    "GET /api/pty/{ptyID}/connect ticket": "D-PTY-TICKET-QUERY",
    "GET /api/experimental/persistent-pty/{ptyID}/connect cursor": "D-PTY-RESUME-CURSOR",
    "GET /api/experimental/persistent-pty/{ptyID}/connect role": "D-PERSISTENT-PTY-PARAMS",
    "GET /api/experimental/persistent-pty/{ptyID}/connect attachment_id": "D-PERSISTENT-PTY-PARAMS",
    "GET /api/experimental/persistent-pty/{ptyID}/connect takeover": "D-PERSISTENT-PTY-PARAMS",
    "GET /api/experimental/persistent-pty/{ptyID}/connect input_protocol": "D-PERSISTENT-PTY-PARAMS",
    "GET /api/experimental/persistent-pty/{ptyID}/connect ticket": "D-PERSISTENT-PTY-PARAMS",
    "GET /api/experimental/client-mcp ticket": "D-CLIENT-MCP-TICKET",
    # Shell output paging: our registry returns the whole bounded buffer
    # (64 KiB cap) and the clients re-fetch rather than page.
    "GET /api/shell/{id}/output cursor": "D-SHELL-OUTPUT-PAGING",
    "GET /api/shell/{id}/output limit": "D-SHELL-OUTPUT-PAGING",
    # Stats windowing: the aggregate covers all history; the TUI's `from`/
    # `timezone` shape the buckets, which we do not compute yet.
    "GET /api/experimental/session/stats from": "D-STATS-WINDOW",
    "GET /api/experimental/session/stats to": "D-STATS-WINDOW",
    "GET /api/experimental/session/stats project": "D-STATS-WINDOW",
    "GET /api/experimental/session/stats timezone": "D-STATS-WINDOW",
    "GET /api/experimental/session/stats tools": "D-STATS-WINDOW",
    # Export redaction and terminal scrollback windows.
    "GET /api/experimental/session/{sessionID}/export sanitize": "D-EXPORT-SANITIZE",
    "GET /api/experimental/session/{sessionID}/terminal/read lines": "D-TERMINAL-READ-LINES",
    # Event type filtering: the endpoint streams every type; the filter would
    # narrow server-side, which no shipped client asks for yet.
    "GET /api/event types": "D-EVENT-TYPE-FILTER",
    # Model type filter (`types=` narrows to tool/text/reasoning entries).
    "GET /api/model types": "D-MODEL-TYPE-FILTER",
    # Project archive filter: projects are not archivable in our store.
    "GET /api/project archived": "D-PROJECT-ARCHIVE",
    # Session diff windowing (`from`/`to`/`context`): the diff is the whole
    # turn snapshot; the TUI passes `context` only to shape git output size.
    "GET /api/session/{sessionID}/diff from": "D-SESSION-DIFF-WINDOW",
    "GET /api/session/{sessionID}/diff to": "D-SESSION-DIFF-WINDOW",
    "GET /api/session/{sessionID}/diff context": "D-SESSION-DIFF-WINDOW",
    # Branch search/limit and diff base/untracked: the TUI sends them; our git
    # helpers return the full branch list and the HEAD-based diff.
    # Worktree listing is not project-scoped in our store.
    "GET /api/worktree projectID": "D-WORKTREE-PROJECT-SCOPE",
})


def router_handlers(text):
    """path -> {method: [handler names]} from `.route(path, get(h).post(h2))`."""
    out = {}
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,\s*', text):
        path = m.group(1)
        depth, i = 1, m.end()
        while i < len(text) and depth:
            if text[i] == "(":
                depth += 1
            elif text[i] == ")":
                depth -= 1
            i += 1
        span = text[m.end():i]
        for meth in ("get", "post", "put", "patch", "delete"):
            for h in re.finditer(rf'(?:^|[^A-Za-z_]){meth}\s*\(\s*([A-Za-z0-9_:]+)', span):
                out.setdefault(path, {}).setdefault(meth.upper(), []).append(
                    h.group(1).split("::")[-1]
                )
    return out


def fn_body(text, name):
    m = re.search(rf'\bfn\s+{re.escape(name)}\s*[<(]', text)
    if not m:
        return None
    # find the opening brace of the signature
    i = text.find("{", m.end())
    if i < 0:
        return None
    depth, j = 1, i + 1
    while j < len(text) and depth:
        c = text[j]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        j += 1
    return text[i:j]


def fn_bodies(text, name):
    """Every definition of `name` (crates may define the same handler name)."""
    out = []
    for m in re.finditer(rf'\bfn\s+{re.escape(name)}\s*[<(]', text):
        i = text.find("{", m.end())
        if i < 0:
            continue
        depth, j = 1, i + 1
        while j < len(text) and depth:
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
            j += 1
        out.append(text[i:j])
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    spec_path, src_paths = args[0], args[1:]
    spec = json.load(open(spec_path))
    sources = "\n".join(open(p).read() for p in src_paths)
    router = router_handlers(sources)

    rows, fail = [], 0
    for path, ops in sorted(spec["paths"].items()):
        op = ops.get("get")
        if not op:
            continue
        declared = [p["name"] for p in op.get("parameters", []) if p.get("in") == "query"]
        if not declared:
            continue
        handlers = router.get(path, {}).get("GET", [])
        if not handlers:
            continue  # binding is rule 16's job
        bodies = []
        for h in handlers:
            bodies.extend(fn_bodies(sources, h))
        joined = "\n".join(bodies)
        if "--explain" in sys.argv and path in (
            "/api/session/{sessionID}/message", "/api/session"
        ):
            print(f"  [explain] {path} handlers={handlers} bodies={[len(b) for b in bodies]}")
        # Read-forms ONLY. A bare `"name"` match is not enough: a response KEY
        # named `cursor` made an unread input look read (found by this gate's
        # own planted control).
        def is_read(name: str) -> bool:
            return bool(
                re.search(rf'q\s*\.\s*get\(\s*"{re.escape(name)}"', joined)
                or re.search(
                    rf'q\s*\.\s*contains_key\(\s*"{re.escape(name)}"', joined
                )
            )

        unread = [d for d in declared if not is_read(d)]
        for d in unread:
            key = f"GET {path} {d}"
            if d == LOCATION_PARAM:
                rows.append((path, d, "cited D-LOCATION-SINGLE-SERVER"))
                continue
            if key in ALLOW:
                rows.append((path, d, f"cited {ALLOW[key]}"))
                continue
            rows.append((path, d, "UNREAD"))
            fail = 1
        if not unread:
            rows.append((path, ",".join(declared), "read"))

    if "--write" in sys.argv:
        with open("bench/openapi/query-params.md", "w") as f:
            f.write("# Declared query parameters vs handler reads\n\n")
            f.write("| route | parameter(s) | verdict |\n|---|---|---|\n")
            for path, params, verdict in rows:
                f.write(f"| `{path}` | {params} | {verdict} |\n")

    bad = [r for r in rows if r[2] == "UNREAD"]
    for path, param, _ in bad:
        print(f"FAIL: {path} declares ?{param}= but the handler never reads it")
    print(f"query-params: {len(rows) - len(bad)} ok, {len(bad)} unread")
    sys.exit(fail)


if __name__ == "__main__":
    main()
