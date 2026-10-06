#!/usr/bin/env python3
"""K2/K3 driver: raw vs normalized hook/behavior parity × {bun, node, deno}.

Entries per plugin:
  raw:    original entry (deno baseline for magic-context = index.deno.js,
          its pre-existing patched raw — documented, not a new patch)
  norm:   *.normalized.mjs (produced by normalizer-spike, same file all runtimes)

Per combo: ping -> load 3 -> trigger chat.message + tool.execute.after +
event x2 -> dispose. Collects hook sets, trigger outputs, stderr lines.
Prints a verdict block: within-runtime raw==norm (K2/K3) and cross-runtime
normalized hook-set identity.
"""
import json
import os
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
REFINE = os.path.expanduser("~/src/refine")
HOST = f"{REFINE}/crates/refine-plugin/src/host/host.mjs"
DENO = "/home/linuxbrew/.linuxbrew/bin/deno"
BUN = f"{HOME}/.bun/bin/bun"
MC = (f"{HOME}/.cache/opencode/packages/@cortexkit/"
      "opencode-magic-context@latest/node_modules/"
      "@cortexkit/opencode-magic-context/dist")
CA = (f"{HOME}/.cache/opencode/packages/@iam-brain/"
      "opencode-codex-auth@latest/node_modules/"
      "@iam-brain/opencode-codex-auth/dist")
REL = f"{HOME}/src/reliary8/opencode-plugin/dist"

SPAWNS = {
    "bun": [BUN, "--smol", HOST],
    "node": ["node", "--max-old-space-size=128", "--max-semi-space-size=2", HOST],
    "deno": [DENO, "run", "-A", "--unstable-ffi", "--node-modules-dir=manual", HOST],
}
RAW_MAGIC = {"bun": f"{MC}/index.js", "node": f"{MC}/index.js",
             "deno": f"{MC}/index.deno.js"}  # deno raw = pre-existing patched
INPUT = {"directory": REFINE, "projectID": "global",
         "serverUrl": "http://127.0.0.1:9"}


class Host:
    def __init__(self, runtime):
        self.p = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True)
        self._id = 0

    def rpc(self, method, params=None, timeout=90):
        self._id += 1
        rid = self._id
        self.p.stdin.write(json.dumps({"id": rid, "method": method,
                                       "params": params}) + "\n")
        self.p.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"eof during {method}")
            m = json.loads(line)
            if m.get("id") == rid:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result")
        raise TimeoutError(method)

    def close(self):
        try:
            self.p.stdin.write(json.dumps({"id": 99999, "method": "dispose"}) + "\n")
            self.p.stdin.flush()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()
        return self.p.stderr.read()


def run_combo(runtime, normalized):
    h = Host(runtime)
    h.rpc("ping")
    entries = [
        ("codex-auth", f"{CA}/index.normalized.mjs" if normalized else f"{CA}/index.js"),
        ("magic-context",
         f"{MC}/index.normalized.mjs" if normalized else RAW_MAGIC[runtime]),
        ("reliary8", f"{REL}/index.normalized.mjs" if normalized else f"{REL}/index.js"),
    ]
    res = {"runtime": runtime, "variant": "norm" if normalized else "raw",
           "hooks": {}, "load_errors": {}}
    for spec, entry in entries:
        try:
            r = h.rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
            res["hooks"][spec] = sorted(r["hooks"])
        except Exception as e:
            res["load_errors"][spec] = str(e)
            res["hooks"][spec] = None
    res["chat"] = h.rpc("trigger", {"name": "chat.message",
                                    "input": {"sessionID": "k3",
                                              "message": {"role": "user",
                                                          "parts": [{"type": "text", "text": "parity"}]}},
                                    "output": {}})
    res["tool"] = h.rpc("trigger", {"name": "tool.execute.after",
                                    "input": {"tool": "write",
                                              "args": {"filePath": "/tmp/parity.rs"}},
                                    "output": {}})
    res["event1"] = h.rpc("event", {"event": {"id": "e1", "type": "session.created",
                                              "properties": {}}})
    res["event2"] = h.rpc("event", {"event": {"id": "e2", "type": "session.created",
                                              "properties": {}}})
    err = h.close()
    res["stderr"] = [l for l in err.splitlines() if l.strip()][:6]
    return res


def main():
    combos = {}
    for rt in ["bun", "node", "deno"]:
        for norm in [False, True]:
            key = f"{rt}-{'norm' if norm else 'raw'}"
            try:
                combos[key] = run_combo(rt, norm)
            except Exception as e:
                combos[key] = {"fatal": str(e)}
            print(f"done {key}", flush=True)
    report = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                          "parity-results.json")
    with open(report, "w") as f:
        json.dump(combos, f, indent=1)

    # ---- verdicts ----
    print("\n=== K2: hook-set parity, raw vs norm, within runtime ===")
    k2 = True
    for rt in ["bun", "node", "deno"]:
        raw, norm = combos.get(f"{rt}-raw", {}), combos.get(f"{rt}-norm", {})
        if raw.get("fatal") or norm.get("fatal"):
            print(f"  {rt}: FATAL raw={raw.get('fatal')} norm={norm.get('fatal')}")
            k2 = False
            continue
        ok = True
        for p in ["codex-auth", "magic-context", "reliary8"]:
            if raw["hooks"].get(p) != norm["hooks"].get(p):
                ok = False
                print(f"  {rt} {p}: MISMATCH raw={raw['hooks'].get(p)} "
                      f"norm={norm['hooks'].get(p)}")
        if norm["load_errors"]:
            ok = False
            print(f"  {rt}: norm load errors {norm['load_errors']}")
        print(f"  {rt}: {'PASS' if ok else 'FAIL'}")
        k2 = k2 and ok

    print("\n=== K2b: normalized hook sets identical ACROSS runtimes ===")
    hs = {rt: combos[f"{rt}-norm"].get("hooks") for rt in ["bun", "node", "deno"]
          if "fatal" not in combos.get(f"{rt}-norm", {})}
    cross = len({json.dumps(v, sort_keys=True) for v in hs.values()}) == 1 and len(hs) == 3
    print(f"  {'PASS' if cross else 'FAIL'} ({len(hs)} runtimes)")

    print("\n=== K3: trigger-output parity, raw vs norm, within runtime ===")
    k3 = True
    for rt in ["bun", "node", "deno"]:
        raw, norm = combos.get(f"{rt}-raw", {}), combos.get(f"{rt}-norm", {})
        if raw.get("fatal") or norm.get("fatal"):
            k3 = False
            continue
        diffs = []
        for k in ["chat", "tool", "event1", "event2"]:
            if raw.get(k) != norm.get(k):
                diffs.append(k)
        if diffs:
            k3 = False
            print(f"  {rt}: DIFF in {diffs}")
            for k in diffs:
                print(f"    raw.{k} = {json.dumps(raw.get(k))[:300]}")
                print(f"    norm.{k} = {json.dumps(norm.get(k))[:300]}")
        else:
            print(f"  {rt}: PASS")

    print("\n=== K4: embedding init on NORMALIZED loads (stderr) ===")
    for rt in ["bun", "node", "deno"]:
        r = combos.get(f"{rt}-norm", {})
        hit = any("embedding model" in l for l in r.get("stderr", []))
        print(f"  {rt}: {'seen' if hit else 'NOT SEEN'} | stderr={r.get('stderr', ['fatal'])[:3]}")

    print(f"\nK2={k2} K2b={cross} K3={k3} -> report {report}")


if __name__ == "__main__":
    main()
