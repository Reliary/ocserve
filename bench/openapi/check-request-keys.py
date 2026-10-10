#!/usr/bin/env python3
"""Rule 20 — handler request-key contract scan (field-contract class guard).

The 2026-10-09 oc-remote double-prompt bug was a request-key divergence:
`prompt.rs` read `messageId` while the frozen wire sends `messageID`
(`prompt.ts:1499 PromptInput`), so client ids were silently dropped.
Typed decode (wire.rs) closed the prompt family at compile time — this
guard extends the net to EVERY remaining axum `Json(...)` handler: every
JSON key READ off an extractor-bound variable must exist as a property or
query parameter in the frozen contract (bench/openapi/1.18.31.json) or be
on the named allowlist.

Scope rules (tuned against the corpus — see FIELD-CONTRACT.md):
  * only reads on variables bound by `Json(<name>)` inside the SAME fn
    (response-building locals named differently are out of scope),
  * assignment targets (`x["k"] = …`) are writes, not reads — excluded,
  * `pointer("/k/…")` checks the FIRST segment only.

Usage: check-request-keys.py <spec.json> <src-dir> [--selftest]
Exit 0 = every request key is contract-valid; 1 = violation(s).
"""
import json
import re
import sys
from pathlib import Path

# additive ocserve routes whose bodies are not in the frozen spec
# (PLAN §16.7: POST /session/search {query, sessionID, limit, offset})
ALLOW = {"offset"}

BIND_RE = re.compile(r"Json\(\s*(\w+)\s*\)")
FN_RE = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)")
GET_RE = re.compile(r'\b(\w+)\.get\("([^"]+)"\)')
PTR_RE = re.compile(r'\b(\w+)\.pointer\("/*([^"/]+)')
IDX_RE = re.compile(r'\b(\w+)\["([^"]+)"\]')


def spec_keys(spec: dict) -> set:
    keys = set()

    def walk(o):
        if isinstance(o, dict):
            for k, v in o.items():
                if k in ("properties", "patternProperties") and isinstance(v, dict):
                    keys.update(v.keys())
                if k == "parameters" and isinstance(v, list):
                    for p in v:
                        if isinstance(p, dict) and p.get("in") in ("query", "path"):
                            keys.add(p.get("name", ""))
                walk(v)
        elif isinstance(o, list):
            for x in o:
                walk(x)

    walk(spec)
    keys.discard("")
    return keys


def functions(text: str):
    """Yield (name, body) for each top-level-ish fn (brace-matched)."""
    for m in FN_RE.finditer(text):
        name = m.group(1)
        # find the opening brace of the body (skip past params/pauses)
        i = m.end()
        depth = 0
        opened = False
        while i < len(text):
            c = text[i]
            if c == "{":
                depth += 1
                opened = True
            elif c == "}":
                depth -= 1
                if opened and depth == 0:
                    yield name, text[m.start() : i + 1]
                    break
            elif c == ";" and not opened:
                # signature-only (trait/extern) — no body
                break
            i += 1


def fn_violations(body: str, allowed: set):
    binds = set(BIND_RE.findall(body))
    if not binds:
        return []
    viol = []
    seen = set()
    for rx, grp in ((GET_RE, 2), (PTR_RE, 2), (IDX_RE, 2)):
        for m in rx.finditer(body):
            var, key = m.group(1), m.group(2)
            if var not in binds:
                continue
            if rx is IDX_RE:
                # skip assignment targets: x["k"] = …  / x["k"] +=
                tail = body[m.end() : m.end() + 3].lstrip()
                if tail.startswith("=") and not tail.startswith("=="):
                    continue
            if key not in allowed and (var, key) not in seen:
                seen.add((var, key))
                viol.append(key)
    return viol


def check(text: str, allowed: set):
    out = []
    for name, body in functions(text):
        for key in fn_violations(body, allowed):
            out.append(f"{name}: request key {key!r} not in frozen contract")
    return out


def selftest(spec: dict, allowed: set) -> int:
    """Prove the scanner is non-vacuous: an injected request-key read on a
    Json-bound variable must be flagged, and the pristine source must not."""
    poisoned = (
        "async fn plant(State(st): State<Arc<AppState>>, Json(payload): Json<Value>) {\n"
        '    let _ = payload.get("messageId");\n'
        "}\n"
    )
    bad = check(poisoned, allowed)
    if not bad:
        print("SELFTEST FAIL: injected payload.get(\"messageId\") not flagged")
        return 1
    print(f"selftest: injected violation correctly flagged -> {bad[0]}")
    # a response-local (NOT Json-bound) read must NOT be flagged
    clean = (
        "async fn resp(Json(body): Json<Value>) {\n"
        '    let info = json!({});\n'
        '    let _ = info.get("whatever_not_in_spec");\n'
        "}\n"
    )
    if check(clean, allowed):
        print("SELFTEST FAIL: non-bound local read was flagged")
        return 1
    print("selftest: non-bound local reads correctly ignored")
    return 0


def main() -> int:
    if len(sys.argv) >= 2 and sys.argv[1] == "--selftest":
        # selftest against a minimal inline spec
        spec = {"components": {"schemas": {"X": {"properties": {"ok": {}}}}}}
        return selftest(spec, {"ok"} | ALLOW)
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    spec = json.loads(Path(sys.argv[1]).read_text())
    allowed = spec_keys(spec) | ALLOW
    src = Path(sys.argv[2])
    files = sorted(src.glob("*.rs"))
    viol = []
    for f in files:
        for v in check(f.read_text(), allowed):
            viol.append(f"{f.name}::{v}")
    if viol:
        print("request-key contract violations (rule 20):")
        for v in viol:
            print(f"  {v}")
        print(
            "\nevery Json-handler request key must exist in the frozen spec\n"
            "(or the named allowlist) — field-probes.md / FIELD-CONTRACT.md"
        )
        return 1
    print(f"request-key contract: {len(files)} handler files clean ({len(allowed)} contract keys)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
