#!/usr/bin/env python3
"""Generate the plugin demand ledger from the last corpus run.

Static only, and labelled as such: the corpus records what a plugin DECLARES
(hooks, tools). It does not prove a hook was ever INVOKED by the model, and for
v2 it does not invoke anything at all (`bench/plugins/check.mjs` reports
"registration only"). Stating that here keeps the ledger honest; the guard
below fails on an UNCLASSIFIED registration, which is the part that catches a
new upstream surface nobody looked at.
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
REPORT = ROOT / "bench/plugins/report.json"
SURFACE = ROOT / "bench/hooks/freeze.surface.json"
OUT = ROOT / "bench/plugins/demand.json"

# Hooks whose dispatch site exists today. Kept deliberately explicit: a new
# upstream hook must be a conscious decision, not an automatic pass.
# `tool` is the v1 plugin-tool REGISTRY (not a hook event); `dispose` is the
# teardown callback — both are bound by the loader path.
V1_BOUND = {
    "chat.message", "chat.params", "chat.headers", "config", "event",
    "experimental.chat.messages.transform", "experimental.chat.system.transform",
    "experimental.session.compacting", "experimental.text.complete",
    "tool.definition", "tool.execute.before", "tool.execute.after", "shell.env",
    "command.execute.before", "dispose", "tool",
}
V2_BOUND = {
    "event", "session.context", "session.compaction", "session.prompt",
    "session.generate", "tool.execute.before", "tool.execute.after",
}
# Named divergences with a reason (README-v2.md capability matrix).
DIVERGED = {
    "auth": "D-HOOK-AUTH-OAUTH (provider OAuth is out of scope; loader-time)",
    "permission.ask": "D-PERM-V2-MODEL (our gate is the authority, not a plugin hook)",
    "session.http.response": "D-V2-SESSION-LIFECYCLE (transport-level Effect hook)",
    "session.model.request": "D-V2-SESSION-LIFECYCLE (transport-level Effect hook)",
    "session.retry": "D-V2-SESSION-LIFECYCLE (retry policy is internal)",
}


def main() -> int:
    report = json.loads(REPORT.read_text())
    surface = set(json.loads(SURFACE.read_text())["surface"])
    led = {}
    for r in report["results"]:
        for hook in r.get("hooks") or []:
            row = led.setdefault(
                hook,
                {"declared_by": [], "dialect": [], "v1_trigger": hook in surface},
            )
            row["declared_by"].append(r["name"])
            d = r.get("dialect") or "?"
            if d not in row["dialect"]:
                row["dialect"].append(d)
    for hook, row in led.items():
        row["declared_by"].sort()
        row["dialect"].sort()
        if hook in V1_BOUND or hook in V2_BOUND:
            row["status"] = "bound"
        elif hook in DIVERGED:
            row["status"] = "diverged"
            row["reason"] = DIVERGED[hook]
        else:
            row["status"] = "UNCLASSIFIED"
    OUT.write_text(json.dumps(dict(sorted(led.items())), indent=2) + "\n")
    bad = [h for h, r in led.items() if r["status"] == "UNCLASSIFIED"]
    if bad:
        print("unclassified plugin surface: " + ", ".join(bad))
        return 1
    print(f"demand ledger: {len(led)} hooks classified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
