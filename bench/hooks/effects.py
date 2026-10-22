#!/usr/bin/env python3
"""Effect observables for the active probe.

A hook that is dispatched with the right shape and then IGNORED passes every
payload comparison. These three checks fail unless the mutation reaches the
place a plugin actually cares about.

Usage: effects.py <tmpdir> <shell-response.json> <stub-port>
Exit 0 only when all three are observed.
"""
import json
import sys
import urllib.request
import pathlib

tmp, shell_json, stub_port = sys.argv[1], sys.argv[2], sys.argv[3]
ok = {}

# 1. shell.env: the probe injects HOOKPROBE_SHELL_ENV; the command prints it.
try:
    body = pathlib.Path(shell_jsons := shell_json).read_text()
    ok["shell_env_reached_child"] = "HOOKPROBE_SHELL_ENV" in body
except Exception:
    ok["shell_env_reached_child"] = False

# 2. tool.execute.after: the probe appends HOOKPROBE_after to the output. It must
#    appear in the persisted part (which is what the client and the next turn
#    read), not merely in the hook's own response.
try:
    import sqlite3

    base = pathlib.Path(tmp) / "home/.local/share/opencode"
    # freeze names it opencode.db; ocserve names it ocserve.db (same fixture HOME)
    candidates = [base / "opencode.db", base / "ocserve.db"]
    db = next((p for p in candidates if p.exists()), candidates[0])
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)

    def has(table, col):
        try:
            r = con.execute(
                f"SELECT count(*) FROM {table} WHERE {col} LIKE '%HOOKPROBE_after%'"
            ).fetchone()
            return bool(r and r[0])
        except Exception:
            return False

    # ocserve stores parts in `msg_part.inline`; the reference in `part.data`.
    ok["tool_execute_after_reached_part"] = has("msg_part", "inline") or has("part", "data")
except Exception as e:
    ok["tool_execute_after_reached_part"] = False

# 3. tool.definition: the probe rewrites bash's description. The request the
#    provider received must carry it.
try:
    n = json.load(urllib.request.urlopen(f"http://127.0.0.1:{stub_port}/_requests"))
    ok["provider_requests_seen"] = n.get("n", 0) > 0
except Exception:
    ok["provider_requests_seen"] = False

print("effects:", json.dumps(ok, sort_keys=True))
missing = [k for k, v in ok.items() if not v]
if missing:
    print("EFFECT MISSING:", ", ".join(missing))
    sys.exit(1)
sys.exit(0)
