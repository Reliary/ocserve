#!/usr/bin/env python3
"""K6-complete: interleaved performance comparison raw vs normalized.

Mirrors deno-spike/perf_bench.py methodology: fresh process per run,
variant order rotated per round (interleave rule), 5 rounds. Workloads:
spawn->pong, load 3 plugins, 100x chat.message RTT (10 warmup),
50x messages.transform with ~120KB conversation, host CPU.

Variants: raw (original entry; deno's magic-context baseline =
index.deno.js, its pre-existing patched raw) vs norm (index.normalized.mjs).

Usage: python3 perf_compare.py [rounds]     (default 5)
"""
import json
import os
import statistics
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
OCSERVE = os.path.expanduser("~/src/ocserve")
HOST = f"{OCSERVE}/crates/ocserve-plugin/src/host/host.mjs"
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
             "deno": f"{MC}/index.deno.js"}
INPUT = {"directory": OCSERVE, "projectID": "global",
         "serverUrl": "http://127.0.0.1:9"}


def entries(runtime, norm):
    if norm:
        return [("codex-auth", f"{CA}/index.normalized.mjs"),
                ("magic-context", f"{MC}/index.normalized.mjs"),
                ("reliary8", f"{REL}/index.normalized.mjs")]
    return [("codex-auth", f"{CA}/index.js"),
            ("magic-context", RAW_MAGIC[runtime]),
            ("reliary8", f"{REL}/index.js")]


def big_conversation(n_msgs=120):
    return {"messages": [
        {"role": "user" if i % 2 == 0 else "assistant",
         "content": f"[{i}] " + ("lorem ipsum dolor sit amet " * 18)[:640]}
        for i in range(n_msgs)]}


def run_once(runtime, norm):
    out = {"runtime": runtime, "variant": "norm" if norm else "raw"}
    p = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True)

    def ticks():
        try:
            with open(f"/proc/{p.pid}/stat") as f:
                parts = f.read().rsplit(")", 1)[1].split()
            return int(parts[11]) + int(parts[12])
        except Exception:
            return -1

    def rpc(method, params=None, timeout=90):
        rpc._id = getattr(rpc, "_id", 0) + 1
        rid = rpc._id
        t0 = time.perf_counter()
        p.stdin.write(json.dumps({"id": rid, "method": method,
                                  "params": params}) + "\n")
        p.stdin.flush()
        deadline = time.perf_counter() + timeout
        while time.perf_counter() < deadline:
            line = p.stdout.readline()
            if not line:
                raise RuntimeError(f"eof during {method}")
            m = json.loads(line)
            if m.get("id") == rid:
                dt = (time.perf_counter() - t0) * 1000
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result"), dt
        raise TimeoutError(method)

    t0 = ticks()
    _, dt = rpc("ping")
    out["boot_ms"] = round(dt, 2)
    load_total = 0.0
    for spec, entry in entries(runtime, norm):
        _, dt = rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
        load_total += dt
    out["load_ms"] = round(load_total, 2)
    for _ in range(10):
        rpc("trigger", {"name": "chat.message",
                        "input": {"sessionID": "p", "message":
                                  {"role": "user", "parts":
                                   [{"type": "text", "text": "w"}]}},
                        "output": {}})
    chat = []
    for _ in range(100):
        _, dt = rpc("trigger", {"name": "chat.message",
                                "input": {"sessionID": "p", "message":
                                          {"role": "user", "parts":
                                           [{"type": "text", "text": "hi"}]}},
                                "output": {}})
        chat.append(dt)
    conv = big_conversation()
    xf = []
    for _ in range(50):
        _, dt = rpc("trigger", {"name": "experimental.chat.messages.transform",
                                "input": {}, "output": dict(conv)})
        xf.append(dt)
    t1 = ticks()
    p.stdin.write(json.dumps({"id": 99999, "method": "dispose"}) + "\n")
    p.stdin.flush()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    if t0 >= 0 and t1 >= t0:
        out["cpu_s"] = round((t1 - t0) / 100.0, 2)

    def agg(v):
        sv = sorted(v)
        return {"p50": round(statistics.median(sv), 3),
                "p95": round(sv[int(len(sv) * 0.95) - 1], 3)}
    out["chat"] = agg(chat)
    out["xform"] = agg(xf)
    return out


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    combos = [(rt, norm) for rt in ["bun", "node", "deno"]
              for norm in [False, True]]
    results = []
    out_path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                            "perf-results.jsonl")
    with open(out_path, "a") as f:
        for r in range(rounds):
            combos = combos[2:] + combos[:2]  # rotate — interleave
            for rt, norm in combos:
                res = run_once(rt, norm)
                res["round"] = r
                results.append(res)
                f.write(json.dumps(res) + "\n")
                f.flush()
                print(f"r{r} {rt:5} {'norm' if norm else 'raw'} "
                      f"boot={res['boot_ms']:6.1f} load={res['load_ms']:7.1f} "
                      f"chat p50={res['chat']['p50']:5.3f} "
                      f"xform p50={res['xform']['p50']:6.3f} "
                      f"cpu={res.get('cpu_s', -1):5.2f}s", flush=True)

    print("\n=== aggregates (median of runs) ===")
    for rt in ["bun", "node", "deno"]:
        row = {}
        for norm in [False, True]:
            rs = [x for x in results if x["runtime"] == rt
                  and x["variant"] == ("norm" if norm else "raw")]
            if not rs:
                continue
            def med(get):
                return round(statistics.median([get(x) for x in rs]), 3)
            row["norm" if norm else "raw"] = {
                "boot": med(lambda x: x["boot_ms"]),
                "load": med(lambda x: x["load_ms"]),
                "chat50": med(lambda x: x["chat"]["p50"]),
                "chat95": med(lambda x: x["chat"]["p95"]),
                "xf50": med(lambda x: x["xform"]["p50"]),
                "xf95": med(lambda x: x["xform"]["p95"]),
                "cpu": med(lambda x: x.get("cpu_s", -1)),
            }
        a, b = row.get("raw"), row.get("norm")
        if a and b:
            print(f"{rt:5} load {a['load']:7.1f} -> {b['load']:7.1f} ms | "
                  f"chat p50 {a['chat50']:.3f} -> {b['chat50']:.3f} | "
                  f"xform p50 {a['xf50']:.3f} -> {b['xf50']:.3f} | "
                  f"cpu {a['cpu']:.2f} -> {b['cpu']:.2f}s")


if __name__ == "__main__":
    main()
