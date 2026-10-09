#!/usr/bin/env python3
"""Event-payload validator (guard rule 19) — the SSE/event surface oracle.

Route-binding (rule 16) proves a route EXISTS; P4 validates JSON 200 BODIES;
but neither sees the **event stream**. Events are where the TUI/web-UI crash
class lives: a `session.updated` carrying a PARTIAL `info` merges into the
client's session store and a renderer reading `r.title.length` throws
(`undefined is not an object`, 2026-10-09). This validator reads every stored
event read-only and checks its `properties` against the frozen contract's
`Event` union (by `type`), so the whole event-payload surface is gated with no
model and no server.

Suffix handling: ocserve stores both the plain type (`session.updated`) and
the sync twin (`session.updated.1`, the durability envelope's `.version`
append). The `.N` suffix is stripped before the spec lookup.

Calibration (same discipline as spec-validate.py): the spec types optional
fields but ocserve legitimately emits `null` (e.g. `model: null`), so
`relax_null` accepts null wherever a type is declared; and additive ocserve
event types not in the frozen union are allowlisted with a citation.

Usage: event-validate.py <db-path>
       event-validate.py --json <frames.json>   (validate a captured frame list)
Exit 0 = every event matches; 1 otherwise.
"""
import json
import os
import re
import sqlite3
import sys
import time

import jsonschema

SPEC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../openapi/1.18.31.json")

# Additive ocserve event types absent from the frozen union, each justified.
# Empty today: ocserve emits only spec types (plus .N twins handled below).
ALLOW_TYPES = set()

SUFFIX = re.compile(r"\.\d+$")


def inline(node, root, stack=()):
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


def event_type_index(spec):
    """type-string -> compiled validator, from the Event union. Validators are
    built ONCE per type (a full scan validates 100k+ events)."""
    idx = {}
    ev = spec["components"]["schemas"].get("Event", {})
    for ref in ev.get("anyOf", []):
        name = ref["$ref"].split("/")[-1]
        sch = spec["components"]["schemas"][name]
        props = sch.get("properties", {})
        tv = props.get("type", {}).get("enum")
        if tv:
            resolved = relax_null(inline(sch, spec))
            prop_schema = resolved.get("properties", {}).get("properties")
            idx[tv[0]] = (
                jsonschema.Draft202012Validator(prop_schema)
                if prop_schema is not None
                else None
            )
    return idx


def validate_payload(props, etype, idx, spec):
    """Return an error string, or None when valid."""
    base = SUFFIX.sub("", etype)
    if base in ALLOW_TYPES:
        return None
    if base not in idx:
        return f"unknown event type {etype!r} (not in the frozen Event union)"
    validator = idx[base]
    if validator is None:
        return None  # event carries no properties (accept)
    errs = sorted(validator.iter_errors(props), key=lambda e: list(e.absolute_path))
    if errs:
        e = errs[0]
        where = "/".join(str(x) for x in e.absolute_path)
        return f"{where or '<root>'}: {str(e.message)[:140]}"
    return None


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: event-validate.py <db-path> [--recent N] | --json <frames.json>")
    spec = json.load(open(SPEC, encoding="utf-8"))
    idx = event_type_index(spec)

    viol = []
    checked = 0
    if sys.argv[1] == "--json":
        doc = json.load(open(sys.argv[2], encoding="utf-8"))
        frames = doc if isinstance(doc, list) else doc.get("frames", [])
        for fr in frames:
            payload = fr.get("payload", fr)
            etype = payload.get("type", "")
            props = payload.get("properties", {})
            checked += 1
            err = validate_payload(props, etype, idx, spec)
            if err:
                viol.append((etype, err))
    else:
        db = sys.argv[1]
        # --recent N: validate only the newest N events.
        # --minutes M: validate only events newer than M minutes ago (the
        #   forward gate — self-heals once post-fix events flow; pre-fix
        #   historical rows age out without reddening a fixed bug).
        recent = 0
        since_ms = None
        i = 2
        while i < len(sys.argv):
            if sys.argv[i] == "--recent":
                recent = int(sys.argv[i + 1])
                i += 2
            elif sys.argv[i] == "--minutes":
                since_ms = int((time.time() - int(sys.argv[i + 1]) * 60) * 1000)
                i += 2
            else:
                sys.exit(f"unknown arg {sys.argv[i]}")
        if not os.path.isfile(db):
            sys.exit(f"no db at {db}")
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        sql = "SELECT type, payload FROM event"
        params = ()
        where = []
        if since_ms is not None:
            where.append("time_created > ?")
            params = (since_ms,)
        if where:
            sql += " WHERE " + " AND ".join(where)
        sql += " ORDER BY seq DESC"
        if recent:
            sql += f" LIMIT {recent}"
        for etype, payload_txt in conn.execute(sql, params):
            try:
                props = json.loads(payload_txt)
            except Exception:
                viol.append((etype, "payload is not JSON"))
                checked += 1
                continue
            checked += 1
            err = validate_payload(props, etype, idx, spec)
            if err:
                viol.append((etype, err))

    print(f"event-validate: checked {checked} events, {len(viol)} violations")
    # group by type for a readable report
    from collections import Counter

    by_type = Counter(t for t, _ in viol)
    for t, n in by_type.most_common():
        sample = next(e for et, e in viol if et == t)
        print(f"  {n:6}  {t}: {sample}")
    return 1 if viol else 0


if __name__ == "__main__":
    sys.exit(main())
