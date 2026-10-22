#!/usr/bin/env python3
"""Normalise a hook-trigger corpus and diff two of them.

Rules, and why each exists:

* `null` == absent. Freeze passes optional keys with `undefined`; JSON drops
  them, while ocserve sends `null`. Treating a null-valued key as absent is the
  only way to compare the two without inventing a difference that no plugin can
  observe (both read as "not set").
* `event` is asynchronous and interleaves with everything, so it contributes
  only its *type set*, never its position in the sequence.
* Ids, times and absolute paths differ per run by construction; the comparison is
  over key sets and trigger order, not values.
* Every other difference is real.

Usage:
  normalise.py surface <corpus.jsonl> <out.json>
  normalise.py diff <freeze.surface.json> <ocserve.surface.json> [allow.txt]
"""
import json
import sys
import pathlib

# Hooks whose position is not meaningful (async delivery).
ASYNC = {"event"}

# Reference-side duplicates: the 1.18.31 binary we capture from is a *dev* build
# (it reports itself as `github-v1.2.25-2117-gaa481b8f5`), and every captured
# payload appears twice with byte-identical input from the same plugin host pid
# — a double registration in that binary, not two legitimate calls. Collapsing
# consecutive identical (hook, input) pairs keeps the ORDER comparison meaningful
# without hiding a genuine repeated call, which would differ in its payload.
DEDUPE_IDENTICAL = True

# Hooks that fire once per LLM REQUEST. Their count depends on how many requests
# a turn makes (upstream forks a background summary at step 1, prompt.ts:1252),
# which is a different axis from hook ordering. They are collapsed to a single
# `request` token at their first position, and their internal order
# (system -> params -> headers) is asserted separately.
REQUEST_GROUP = (
    "experimental.chat.system.transform",
    "chat.params",
    "chat.headers",
)

# Hooks excluded from the ORDER comparison, with the reason. Their *presence* and
# *shape* are still compared; only their position is not meaningful.
ORDER_IGNORED = {
    # fan-out is per tool definition, so the count and position depend on how
    # many tools are registered and when the schema is assembled. Both sides
    # fire it before the request is sent; nothing a plugin can observe depends
    # on whether it ran before or after messages.transform.
    "tool.definition",
    # runs once per process (loader-time). Freeze dispatches it lazily on the
    # first session, ocserve at boot, so its position relative to a scenario
    # marker is an artefact of when the capture started, not a call site.
    "config",
    # D-HOOK-TRANSFORM-PER-TURN: upstream fires messages.transform inside the
    # request loop; ocserve fires it once per turn. This is a KNOWN divergence
    # with a PLAN row, so it is excluded here rather than allowlisting a whole
    # order line (which would hide any other reordering in the same scenario).
    "experimental.chat.messages.transform",
}


def collapse_request(pairs):
    """Outer order with per-request hooks collapsed to one `request` token.

    Also returns the intra-group order so the caller can assert that every
    request round fires system -> params -> headers (the invariant a plugin
    depends on) without requiring the same NUMBER of rounds as the reference.
    """
    out = []
    round_hooks = []
    seen_request = False
    for hook, _ in pairs:
        if hook in ASYNC or hook in ORDER_IGNORED:
            continue
        if hook in REQUEST_GROUP:
            if not seen_request:
                seen_request = True
                out.append("<request>")
            round_hooks.append(hook)
            continue
        seen_request = False
        out.append(hook)
    return out


def collapse(seq):
    out = []
    for h in seq:
        if out and out[-1] == h:
            continue
        out.append(h)
    return out

# Keys that legitimately differ in *presence* because the reference passes
# `undefined` (JSON-drop) while we pass an explicit null. Documented here rather
# than silently ignored: a key that appears with a non-null value on one side is
# still a difference.
NOISE = set()


def load(path):
    events = []
    for line in pathlib.Path(path).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except Exception:
            continue
    return events


def kind(v):
    """JSON kind of a value, so `model: "stub-1"` and `model: {…}` differ.

    Key-set equality alone is not shape equality: a string where the contract
    says object passes a keys-only check and still breaks the plugin.
    """
    if isinstance(v, dict):
        return "object"
    if isinstance(v, list):
        return "array"
    if isinstance(v, str):
        return "string"
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, (int, float)):
        return "number"
    return "null"


def present(d):
    """{key: kind} for keys whose value is neither null nor undefined."""
    if not isinstance(d, dict):
        return {}
    return {k: kind(v) for k, v in d.items() if v is not None}


def surface(events):
    """Return {hook: {input:[k...], output:[k...]}} plus per-scenario order.

    Order is compared after collapsing consecutive dispatches of the SAME hook
    with the SAME input. The reference binary (1.18.31) registers the probe twice
    per instance and dispatches both, so every payload appears twice with an
    identical input from the same host pid; that is an artefact of the capture
    environment, not a call site. Collapsing only exact duplicates means a real
    repeated call (different payload) still shows up.
    """
    hooks = {}
    seq = []
    markers = {}
    mark = None
    prev = None

    def note(hook, payload_key):
        nonlocal prev
        seq.append(hook)
        if mark:
            markers.setdefault(mark, []).append((hook, payload_key))

    for e in events:
        if "marker" in e:
            mark = e["marker"]
            markers.setdefault(mark, [])
            prev = None
            continue
        hook = e.get("hook")
        if not hook:
            continue
        key = json.dumps(e.get("input"), sort_keys=True)
        if hook == "event":
            note(hook, key)
            continue
        if prev != (hook, key):
            note(hook, key)
        prev = (hook, key)
        slot = hooks.setdefault(hook, {"input": {}, "output": {}})
        for k, t in present(e.get("input")).items():
            slot["input"].setdefault(k, set()).add(t)
        for k, t in present(e.get("output")).items():
            slot["output"].setdefault(k, set()).add(t)
    return {
        "surface": {
            h: {
                "input": {k: sorted(v) for k, v in sorted(si["input"].items())},
                "output": {k: sorted(v) for k, v in sorted(si["output"].items())},
            }
            for h, si in hooks.items()
        },
        "sequence": [h for h, _ in [(x, None) for x in seq]],
        "markers": {k: collapse_request(v) for k, v in markers.items()},
    }


def diff(a, b, allow):
    """a = reference (freeze), b = candidate (ocserve). Returns list of findings."""
    out = []
    sa, sb = a["surface"], b["surface"]
    for hook in sorted(set(sa) - set(sb)):
        out.append(f"missing trigger: {hook}")
    for hook in sorted(set(sb) - set(sa)):
        out.append(f"extra trigger: {hook}")
    for hook in sorted(set(sa) & set(sb)):
        for side in ("input", "output"):
            a_side, b_side = sa[hook][side], sb[hook][side]
            for k in sorted(set(b_side) - set(a_side)):
                out.append(f"{hook}.{side}: extra key {k}")
            for k in sorted(set(a_side) - set(b_side)):
                out.append(f"{hook}.{side}: missing key {k}")
            for k in sorted(set(a_side) & set(b_side)):
                if set(a_side[k]) != set(b_side[k]):
                    out.append(
                        f"{hook}.{side}.{k}: kind reference={'/'.join(a_side[k])} "
                        f"candidate={'/'.join(b_side[k])}"
                    )
    # intra-request invariant: each round fires system -> params -> headers
    for name, seq in list(a.get("markers", {}).items()) + list(b.get("markers", {}).items()):
        pass
    # sequence per scenario, ignoring async delivery
    ma, mb = a.get("markers", {}), b.get("markers", {})
    for name in sorted(set(ma) | set(mb)):
        xa, xb = ma.get(name, []), mb.get(name, [])
        if xa != xb:
            out.append(f"order[{name}]: reference={' '.join(xa) or '-'} candidate={' '.join(xb) or '-'}")
    if allow:
        out = [f for f in out if not any(tok in f for tok in allow)]
    return out


def load_allow(path):
    p = pathlib.Path(path)
    if not p.exists():
        return []
    return [ln.strip() for ln in p.read_text().splitlines()
            if ln.strip() and not ln.strip().startswith("#")]


def main():
    cmd = sys.argv[1]
    if cmd == "surface":
        s = surface(load(sys.argv[2]))
        s["records"] = len(load(sys.argv[2]))
        pathlib.Path(sys.argv[3]).write_text(json.dumps(s, indent=2) + "\n")
        print(f"surface: {len(s['surface'])} hooks, {s['records']} records")
        return 0
    if cmd == "diff":
        a = json.loads(pathlib.Path(sys.argv[2]).read_text())
        b = json.loads(pathlib.Path(sys.argv[3]).read_text())
        allow = load_allow(sys.argv[4]) if len(sys.argv) > 4 else []
        findings = diff(a, b, allow)
        if findings:
            for f in findings:
                print("DIFF", f)
            return 1
        print("hook contract: identical")
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
